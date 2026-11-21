//! Mapping of PostgreSQL failures onto the contract's [`ErrorKind`]s (ADR-26, ADR-45).
//!
//! The whole SQLSTATE table is [`kind_for_sqlstate`]. Messages are plain language; the driver's
//! own text never reaches [`StorageError::explanation`]. It is logged at `debug` level instead,
//! because the contract's error type carries no source chain (see the task report).

use spot_storage_contract::{Catalog, ErrorKind, Name, StorageError, Wait};

use crate::schema::names::link_for_constraint;
use tokio_postgres::error::{DbError, Severity};

/// Kinds after which a transaction may continue, but only when the failed operation ran inside
/// a savepoint that was rolled back (the rule in the contract's `Transaction` documentation).
pub(crate) const RECOVERABLE: [ErrorKind; 10] = [
    ErrorKind::NotFound,
    ErrorKind::StaleVersion,
    ErrorKind::AlreadyExists,
    ErrorKind::UniqueViolation,
    ErrorKind::ExclusionViolation,
    ErrorKind::InvalidValue,
    ErrorKind::InvalidQuery,
    ErrorKind::UnknownEntity,
    ErrorKind::UnknownField,
    ErrorKind::LockNotAvailable,
];

/// Whether a transaction may continue after `kind` (inside a rolled-back savepoint).
pub(crate) fn is_recoverable(kind: ErrorKind) -> bool {
    RECOVERABLE.contains(&kind)
}

/// The SQLSTATE table. `lock_wait` tells a `NOWAIT` refusal from a `lock_timeout` expiry, which
/// PostgreSQL reports with the same code `55P03`.
///
/// The caller knows which it asked for: an operation that took its lock with
/// [`Wait::NoWait`] passes `Some(Wait::NoWait)` and gets `LockNotAvailable`; every other
/// `55P03` is the transaction's `lock_timeout` and gets `LockTimeout`. The server's message text
/// is deliberately not used: it is localised.
pub fn kind_for_sqlstate(code: &str, lock_wait: Option<Wait>) -> ErrorKind {
    match code {
        "40001" => ErrorKind::SerializationFailure,
        "40P01" => ErrorKind::Deadlock,
        "55P03" if lock_wait == Some(Wait::NoWait) => ErrorKind::LockNotAvailable,
        "55P03" => ErrorKind::LockTimeout,
        "57014" => ErrorKind::StatementTimeout,
        "23505" => ErrorKind::UniqueViolation,
        "23P01" => ErrorKind::ExclusionViolation,
        "23503" => ErrorKind::ReferenceViolation,
        "23502" | "23514" => ErrorKind::InvalidValue,
        "25006" => ErrorKind::ReadOnlyTransaction,
        "57P01" | "57P02" | "57P03" | "57P05" => ErrorKind::Unavailable,
        c if c.starts_with("22") => ErrorKind::InvalidValue,
        c if c.starts_with("08") || c.starts_with("53") => ErrorKind::Unavailable,
        _ => ErrorKind::Internal,
    }
}

/// Maps a driver error to a storage error. `lock_wait` is explained at [`kind_for_sqlstate`].
///
/// An error with no server response (closed or reset connection, I/O failure, timeout) is
/// `Unavailable`; the caller decides whether a commit in flight makes it `CommitOutcomeUnknown`.
pub(crate) fn map_pg_error(err: &tokio_postgres::Error, lock_wait: Option<Wait>) -> StorageError {
    tracing::debug!(error = ?err, "postgres error");
    match err.as_db_error() {
        Some(db) => map_db_error(db, lock_wait),
        None if err.is_closed() => StorageError::new(
            ErrorKind::Unavailable,
            "the connection to the database was lost; nothing was committed by this statement",
        ),
        None => StorageError::new(
            ErrorKind::Unavailable,
            "the database could not be reached or did not answer; nothing was committed by this statement",
        ),
    }
}

fn map_db_error(db: &DbError, lock_wait: Option<Wait>) -> StorageError {
    let code = db.code().code();
    let kind = kind_for_sqlstate(code, lock_wait);
    let explanation = match kind {
        ErrorKind::SerializationFailure => {
            "the transaction conflicted with a concurrent one and could not be serialized; retry it"
                .to_owned()
        }
        ErrorKind::Deadlock => {
            "the transaction was chosen as a deadlock victim; retry it".to_owned()
        }
        ErrorKind::LockTimeout => {
            "a lock was not granted within the transaction's lock timeout".to_owned()
        }
        ErrorKind::LockNotAvailable => "the lock is held by another transaction".to_owned(),
        ErrorKind::StatementTimeout => {
            "the statement exceeded the transaction's statement timeout".to_owned()
        }
        ErrorKind::UniqueViolation => "a record with the same unique key already exists".to_owned(),
        // The server's own detail names the conflicting key values; it is never used.
        ErrorKind::ExclusionViolation => {
            "the value conflicts with an existing record under an exclusion rule".to_owned()
        }
        ErrorKind::ReferenceViolation => {
            "a link points at a record that does not exist, or the record is still linked"
                .to_owned()
        }
        ErrorKind::InvalidValue => "a value does not fit its field".to_owned(),
        ErrorKind::ReadOnlyTransaction => {
            "a write was attempted in a read-only transaction".to_owned()
        }
        ErrorKind::Unavailable => "the database is not available right now".to_owned(),
        _ => format!("unexpected database error (SQLSTATE {code})"),
    };
    let mut error = StorageError::new(kind, explanation);
    error.entity = db.table().and_then(|s| Name::new(s).ok());
    error.field = db.column().and_then(|s| Name::new(s).ok());
    error.constraint = db.constraint().and_then(|s| Name::new(s).ok());
    error
}

/// Maps the failure of `COMMIT`: any connection-level failure while the commit was in flight is
/// `CommitOutcomeUnknown`, never `Unavailable`, because the server may have committed before the
/// connection died. A server `ErrorResponse` of severity `ERROR` is a definite answer (the
/// transaction was rolled back) and goes through the normal table.
pub(crate) fn map_commit_error(err: &tokio_postgres::Error, catalog: &Catalog) -> StorageError {
    match err.as_db_error() {
        Some(db) if !is_connection_level(db) => {
            let mut mapped = map_db_error(db, None);
            // PostgreSQL's foreign key error has no column: name the field from the constraint.
            let link = db
                .constraint()
                .filter(|_| mapped.kind == ErrorKind::ReferenceViolation)
                .and_then(|c| link_for_constraint(catalog, c));
            if let Some((entity, field)) = link {
                mapped.entity = Some(entity);
                mapped.field = Some(field);
            }
            mapped
        }
        _ => {
            tracing::debug!(error = ?err, "postgres error during commit");
            commit_outcome_unknown()
        }
    }
}

/// The error for a commit whose answer never arrived.
pub(crate) fn commit_outcome_unknown() -> StorageError {
    StorageError::new(
        ErrorKind::CommitOutcomeUnknown,
        "the connection failed while committing: the transaction may or may not have been \
         committed; check the outcome before retrying",
    )
}

/// Fatal responses and class 08 / 57P0x codes end the connection, so the commit's fate is unknown.
fn is_connection_level(db: &DbError) -> bool {
    let code = db.code().code();
    matches!(
        db.parsed_severity(),
        Some(Severity::Fatal | Severity::Panic)
    ) || code.starts_with("08")
        || code.starts_with("57P")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_maps_the_documented_states() {
        let cases = [
            ("40001", ErrorKind::SerializationFailure),
            ("40P01", ErrorKind::Deadlock),
            ("55P03", ErrorKind::LockTimeout),
            ("57014", ErrorKind::StatementTimeout),
            ("23505", ErrorKind::UniqueViolation),
            ("23P01", ErrorKind::ExclusionViolation),
            ("23503", ErrorKind::ReferenceViolation),
            ("23502", ErrorKind::InvalidValue),
            ("22003", ErrorKind::InvalidValue),
            ("22P02", ErrorKind::InvalidValue),
            ("25006", ErrorKind::ReadOnlyTransaction),
            ("08006", ErrorKind::Unavailable),
            ("08001", ErrorKind::Unavailable),
            ("57P01", ErrorKind::Unavailable),
            ("57P02", ErrorKind::Unavailable),
            ("57P03", ErrorKind::Unavailable),
            ("53300", ErrorKind::Unavailable),
            ("53100", ErrorKind::Unavailable),
            ("42601", ErrorKind::Internal),
            ("XX000", ErrorKind::Internal),
        ];
        for (code, kind) in cases {
            assert_eq!(kind_for_sqlstate(code, None), kind, "SQLSTATE {code}");
        }
    }

    #[test]
    fn nowait_and_lock_timeout_share_a_state_but_not_a_kind() {
        assert_eq!(
            kind_for_sqlstate("55P03", Some(Wait::NoWait)),
            ErrorKind::LockNotAvailable
        );
        assert_eq!(
            kind_for_sqlstate("55P03", Some(Wait::Block)),
            ErrorKind::LockTimeout
        );
        assert_eq!(kind_for_sqlstate("55P03", None), ErrorKind::LockTimeout);
    }

    #[test]
    fn retryable_kinds_follow_the_contract() {
        assert!(kind_for_sqlstate("40001", None).is_retryable());
        assert!(kind_for_sqlstate("40P01", None).is_retryable());
        assert!(!kind_for_sqlstate("23505", None).is_retryable());
    }

    #[test]
    fn recoverable_set_is_the_contracts() {
        assert!(is_recoverable(ErrorKind::UniqueViolation));
        assert!(is_recoverable(ErrorKind::LockNotAvailable));
        assert!(!is_recoverable(ErrorKind::StatementTimeout));
        assert!(!is_recoverable(ErrorKind::SerializationFailure));
        assert!(!is_recoverable(ErrorKind::ReferenceViolation));
    }
}
