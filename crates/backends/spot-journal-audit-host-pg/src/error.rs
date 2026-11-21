//! Typed errors with stable `journal_audit_host.*` codes. A message never carries a password, a
//! connection string, a record value or a key.

/// Why part of the hosted audit could not be done.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// The database worker thread is gone or could not start.
    #[error("the audit database worker is not running")]
    Worker,
    /// A statement failed; carries the SQLSTATE or a fixed sentence, never the database's text.
    #[error("the database refused the statement ({0})")]
    Db(String),
    /// The scratch role holds a privilege beyond `CREATEDB`.
    #[error("the scratch role holds more than CREATEDB: {0}")]
    RoleTooPowerful(&'static str),
    /// The scratch role does not exist or cannot create a database.
    #[error("the scratch role cannot create databases")]
    RoleUnusable,
    /// A tenant id or run id cannot name a scratch database.
    #[error("the tenant or run id cannot name a scratch database")]
    ScratchName,
    /// The scratch database could not be created, secured or dropped.
    #[error("the scratch database could not be {0}")]
    Scratch(&'static str),
    /// The inputs asked for belong to another tenant than the one the call named.
    #[error("the inputs belong to another tenant")]
    TenantMismatch,
    /// The journal source (store, keys, options) of the tenant cannot be opened.
    #[error("the journal source of the tenant cannot be opened")]
    Source,
    /// A stored result row does not decode.
    #[error("a stored audit result does not decode")]
    ResultInvalid,
}

impl HostError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Worker => "journal_audit_host.worker_failed",
            Self::Db(_) => "journal_audit_host.db_failed",
            Self::RoleTooPowerful(_) => "journal_audit_host.role_too_powerful",
            Self::RoleUnusable => "journal_audit_host.role_unusable",
            Self::ScratchName => "journal_audit_host.scratch_name_invalid",
            Self::Scratch(_) => "journal_audit_host.scratch_failed",
            Self::TenantMismatch => "journal_audit_host.tenant_mismatch",
            Self::Source => "journal_audit_host.source_unavailable",
            Self::ResultInvalid => "journal_audit_host.result_invalid",
        }
    }
}

/// A database error reduced to its SQLSTATE (or a fixed word): the message can carry a value.
pub(crate) fn db_error(error: &tokio_postgres::Error) -> HostError {
    HostError::Db(
        error
            .code()
            .map_or_else(|| "no SQLSTATE".to_owned(), |c| c.code().to_owned()),
    )
}
