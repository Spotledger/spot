//! Turning database, pool and schema failures into [`RetrievalError`] and [`PrepareError`].
//!
//! A read or a write through [`VectorStore`](spot_retrieval::VectorStore) is answered with
//! `RetrievalError::Store { detail }` (the stable code `retrieval.store_failed`). The detail
//! names what was refused and a short cause, and **never** a value: a chunk's text, a vector, a
//! tenant, a database name, a password or the driver's own message, which can quote a row or a
//! connection string. Schema preparation has its own small enum because "the server has no
//! pgvector" is the one condition a caller may legitimately skip on.

use std::fmt;

use deadpool_postgres::{PoolError, TimeoutType};
use spot_retrieval::RetrievalError;

/// The SQLSTATE of a driver error, or `none` for a connection-level failure.
fn sqlstate(error: &tokio_postgres::Error) -> &str {
    error.code().map_or("none", |state| state.code())
}

/// A stable short cause for a SQLSTATE, chosen from the class of the failure only.
fn cause_for(state: &str) -> &'static str {
    match state {
        "none" => "connection_lost",
        "40P01" | "40001" => "conflict",
        "57014" => "timeout",
        "42501" => "permission_denied",
        "23505" => "duplicate",
        "23514" => "constraint",
        "22P02" | "22023" | "22000" => "bad_value",
        other if other.starts_with("08") || other.starts_with("57P") => "connection_lost",
        other if other.starts_with("42") => "statement_refused",
        other if other.starts_with("23") => "constraint",
        _ => "statement_failed",
    }
}

/// A database failure while running a statement, as a store refusal with a short cause code.
pub(crate) fn store(error: &tokio_postgres::Error) -> RetrievalError {
    RetrievalError::Store {
        detail: format!(
            "the vector database refused a statement ({})",
            cause_for(sqlstate(error))
        ),
    }
}

/// A pool failure (no connection could be checked out).
pub(crate) fn store_pool(error: &PoolError) -> RetrievalError {
    RetrievalError::Store {
        detail: format!(
            "the vector database is unavailable ({})",
            match error {
                PoolError::Timeout(TimeoutType::Wait) => "pool_exhausted",
                PoolError::Timeout(_) => "timeout",
                PoolError::Backend(_) | PoolError::PostCreateHook(_) => "connection_lost",
                PoolError::Closed | PoolError::NoRuntimeSpecified => "pool_closed",
            }
        ),
    }
}

/// The store refused a call before touching the database (a bad width, a value that does not
/// fit, a row that could not be decoded). `detail` is a fixed sentence written in this crate.
pub(crate) fn refused(detail: &str) -> RetrievalError {
    RetrievalError::Store {
        detail: detail.to_owned(),
    }
}

/// Why [`PgVectorStore::prepare`](crate::PgVectorStore::prepare) did not finish.
#[derive(Debug)]
pub enum PrepareError {
    /// The server does not offer the `vector` extension. A test turns this into a `SKIPPED:`
    /// line; production reports it with the fix: install the pgvector package on the server.
    ExtensionMissing,
    /// The table already exists with another vector width than this store was built for. The
    /// width is fixed per index policy: change it only by re-indexing into a new table.
    DimensionMismatch {
        /// The width the existing column has.
        stored: i32,
        /// The width this store was built for.
        wanted: usize,
    },
    /// The database refused the DDL or no connection was available.
    Failed(RetrievalError),
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExtensionMissing => f.write_str(
                "the PostgreSQL server has no `vector` extension available; install the pgvector \
                 package on the server and prepare the retrieval index again",
            ),
            Self::DimensionMismatch { stored, wanted } => write!(
                f,
                "the retrieval table has {stored}-wide vectors but this store is configured for \
                 {wanted}; re-index into a new table instead of changing the width in place"
            ),
            Self::Failed(error) => write!(f, "the retrieval tables could not be prepared: {error}"),
        }
    }
}

impl std::error::Error for PrepareError {}

impl From<PrepareError> for RetrievalError {
    fn from(error: PrepareError) -> Self {
        RetrievalError::Store {
            detail: error.to_string(),
        }
    }
}

/// A failed DDL statement.
pub(crate) fn prepare(error: &tokio_postgres::Error) -> PrepareError {
    PrepareError::Failed(RetrievalError::Store {
        detail: format!(
            "the retrieval DDL was refused ({})",
            cause_for(sqlstate(error))
        ),
    })
}

/// A pool failure while preparing the schema.
pub(crate) fn prepare_pool(error: &PoolError) -> PrepareError {
    PrepareError::Failed(store_pool(error))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_store_refusal_has_the_stable_code_and_names_no_sql() {
        let refused = refused("the vector has the wrong width");
        assert_eq!(refused.code(), "retrieval.store_failed");
        let text = refused.to_string();
        assert!(!text.contains("SELECT"), "{text}");
    }

    #[test]
    fn pool_failures_map_to_a_cause_code() {
        for (error, cause) in [
            (PoolError::Timeout(TimeoutType::Wait), "pool_exhausted"),
            (PoolError::Timeout(TimeoutType::Create), "timeout"),
            (PoolError::Closed, "pool_closed"),
        ] {
            let mapped = store_pool(&error).to_string();
            assert!(mapped.contains(cause), "{mapped}");
        }
    }

    #[test]
    fn sqlstates_map_to_causes_by_class() {
        assert_eq!(cause_for("42501"), "permission_denied");
        assert_eq!(cause_for("08006"), "connection_lost");
        assert_eq!(cause_for("42P01"), "statement_refused");
        assert_eq!(cause_for("XX000"), "statement_failed");
    }

    #[test]
    fn a_prepare_error_converts_to_a_store_failure_with_the_fix_text() {
        let error: RetrievalError = PrepareError::ExtensionMissing.into();
        assert_eq!(error.code(), "retrieval.store_failed");
        assert!(error.to_string().contains("pgvector"));
        let mismatch = PrepareError::DimensionMismatch {
            stored: 8,
            wanted: 16,
        }
        .to_string();
        assert!(mismatch.contains('8') && mismatch.contains("16"));
    }
}
