//! The failures of the PostgreSQL sides, with a stable `journal_audit_pg.*` code each.
//!
//! A message names a thing (an entity, a schema, a bridge code, a SQLSTATE) and never a record
//! value, a statement's parameters or a connection string: the sides run inside the scheduled
//! audit of a live tenant, whose results are read by operators and kept.

use std::fmt;

use spot_pg_bridge::BridgeError;

/// The side belongs to another tenant than the call asked for, or the tenant name is not a
/// storage name.
pub const TENANT_MISMATCH: &str = "journal_audit_pg.tenant_mismatch";

/// The options a side was opened with cannot hold: no entity to read, a page size of zero, or a
/// fingerprint declaration that names an empty field.
pub const OPTIONS_INVALID: &str = "journal_audit_pg.options_invalid";

/// The held session ended, so the side can no longer reach its database: the bridge refused or
/// timed out, or the worker stopped.
pub const SESSION_FAILED: &str = "journal_audit_pg.session_failed";

/// A statement of a side failed. The message carries the SQLSTATE only.
pub const QUERY_FAILED: &str = "journal_audit_pg.query_failed";

/// The change feed's head cannot be read: the feed is not installed in the tenant database, or
/// its state row is missing.
pub const FEED_MISSING: &str = "journal_audit_pg.feed_missing";

/// The call asked for a position other than the one recorded when the live side was opened. The
/// live database cannot be read in the past.
pub const AS_OF_NOT_HEAD: &str = "journal_audit_pg.as_of_not_head";

/// A statement tried to write through the live side's connection; its transaction is read-only.
pub const READ_ONLY: &str = "journal_audit_pg.read_only";

/// An image the rebuild handed the scratch side cannot be stored: an unknown entity, a child row
/// without an id, or a value whose shape contradicts its declared type.
pub const IMAGE_INVALID: &str = "journal_audit_pg.image_invalid";

/// The scratch schema cannot be created or its name is unusable: not a storage name, longer than
/// PostgreSQL's identifier limit, or already present.
pub const SCRATCH_INVALID: &str = "journal_audit_pg.scratch_invalid";

/// The sweep could not list or drop a leftover scratch schema.
pub const SWEEP_FAILED: &str = "journal_audit_pg.sweep_failed";

/// Why one of the PostgreSQL sides refused to answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgSideError {
    code: &'static str,
    message: String,
}

impl PgSideError {
    /// The stable dotted code: one of this module's constants.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The sentence, with no record value in it.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PgSideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PgSideError {}

pub(crate) fn error(code: &'static str, message: impl Into<String>) -> PgSideError {
    PgSideError {
        code,
        message: message.into(),
    }
}

pub(crate) fn tenant_mismatch(side: &str) -> PgSideError {
    error(
        TENANT_MISMATCH,
        format!("the {side} side belongs to another tenant than this call names"),
    )
}

pub(crate) fn options_invalid(reason: &str) -> PgSideError {
    error(OPTIONS_INVALID, reason.to_owned())
}

pub(crate) fn feed_missing(detail: &str) -> PgSideError {
    error(
        FEED_MISSING,
        format!("the change feed's head cannot be read: {detail}"),
    )
}

pub(crate) fn as_of_not_head(head: u64) -> PgSideError {
    error(
        AS_OF_NOT_HEAD,
        format!(
            "the live side is pinned to the head at position {head}; the live database cannot be \
             read at another position"
        ),
    )
}

pub(crate) fn image_invalid(reason: &str) -> PgSideError {
    error(IMAGE_INVALID, reason.to_owned())
}

pub(crate) fn scratch_invalid(reason: &str) -> PgSideError {
    error(SCRATCH_INVALID, reason.to_owned())
}

pub(crate) fn sweep_failed(detail: &str) -> PgSideError {
    error(
        SWEEP_FAILED,
        format!("the leftover scratch schemas could not be swept: {detail}"),
    )
}

/// A `tokio_postgres` failure as a [`QUERY_FAILED`], carrying its SQLSTATE and never its text
/// (the driver's message can quote a parameter value).
pub(crate) fn query_failed(failure: &tokio_postgres::Error) -> PgSideError {
    match failure.code() {
        Some(state) if state.code() == "25006" => error(
            READ_ONLY,
            "a statement tried to write through the live side, whose transaction is read-only",
        ),
        Some(state) => error(
            QUERY_FAILED,
            format!("a statement failed with SQLSTATE {}", state.code()),
        ),
        None => error(QUERY_FAILED, "a statement failed without a SQLSTATE"),
    }
}

/// A bridge refusal as a [`SESSION_FAILED`], carrying the bridge's own code when it has one.
/// [`BridgeError::Store`] is the side's own error and passes through unchanged.
pub(crate) fn session_failed(refusal: &BridgeError<PgSideError>) -> PgSideError {
    match refusal {
        BridgeError::Store(inner) => inner.clone(),
        other => error(
            SESSION_FAILED,
            match other.code() {
                Some(code) => format!("the held session ended ({code})"),
                None => "the held session ended".to_owned(),
            },
        ),
    }
}

/// A `tokio_postgres` failure inside the held session, as [`SESSION_FAILED`] with its SQLSTATE: a
/// broken connection is not a statement refusal.
pub(crate) fn session_lost(failure: &tokio_postgres::Error) -> PgSideError {
    match failure.code() {
        Some(state) => error(
            SESSION_FAILED,
            format!(
                "the held session's statement failed with SQLSTATE {}",
                state.code()
            ),
        ),
        None => error(
            SESSION_FAILED,
            "the held session's connection failed without a SQLSTATE",
        ),
    }
}
