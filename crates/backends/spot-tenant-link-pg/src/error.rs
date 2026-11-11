//! Errors: the two `tenant_link_pg.*` codes this backend adds, and the mapping of database
//! failures onto the fixed-reason errors the `spot-tenant-link` traits carry.
//!
//! The [`LinkStore`](spot_tenant_link::LinkStore) trait returns [`spot_tenant_link::Error`],
//! whose `StoreFailed` reason is a `&'static str`. No failure therefore carries a raw SQL
//! string, a connection string, a driver message or a credential: a driver or pool failure is
//! reported as `tenant_link.store_failed` with a fixed sentence, and the catalogue explains it.

use std::fmt;

use deadpool_postgres::PoolError;
use spot_app_api::{AppError, ErrorClass};
use spot_tenant_link::Error;

/// The tenant schema or the control registry could not be created: a `tenant_link_pg.*` code.
pub const MIGRATION_FAILED: &str = "tenant_link_pg.migration_failed";

/// The caller lacks the `link.manage` right for an administration operation.
pub const PERMISSION_DENIED: &str = "tenant_link_pg.permission_denied";

/// A failed statement while creating a schema.
pub(crate) fn migration(error: tokio_postgres::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        format!(
            "the tenant link tables could not be created: {}",
            describe(&error)
        ),
    )
}

/// What a database error says without any row value: the message and the SQLSTATE, never the
/// detail (which can quote the key of a row).
fn describe(error: &tokio_postgres::Error) -> String {
    error.as_db_error().map_or_else(
        || "the database call failed".to_owned(),
        |db| format!("{} (SQLSTATE {})", db.message(), db.code().code()),
    )
}

/// A schema migration this crate refuses to guess at (both names of a table present, an old table
/// that is not ours, or a name already taken). The message names tables only, never a row.
pub(crate) fn migration_refused(message: String) -> AppError {
    AppError::new(ErrorClass::Internal, MIGRATION_FAILED, message)
}

/// A pool failure while creating a schema.
pub(crate) fn migration_pool(_error: PoolError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        MIGRATION_FAILED,
        "the tenant link tables could not be created: no connection is available",
    )
}

/// A store failure with a fixed reason.
pub(crate) fn store_failed(reason: &'static str) -> Error {
    Error::StoreFailed { reason }
}

/// The failure for a driver error on a read or write.
pub(crate) fn query_failed(_error: tokio_postgres::Error) -> Error {
    store_failed("the link store query failed")
}

/// Whether a driver error is a unique-constraint violation (`SQLSTATE 23505`).
pub(crate) fn is_unique_violation(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|db| db.code().code() == "23505")
}

/// What an administration operation refuses with.
#[derive(Debug)]
pub enum OpError {
    /// The link layer refused: a validation, a transition, a version conflict, a store or an
    /// audit failure (the `tenant_link.*` codes).
    Link(Error),
    /// The caller does not hold the `link.manage` right; the denial was audited.
    Denied {
        /// The operation that was refused.
        operation: &'static str,
    },
}

impl OpError {
    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Link(error) => error.code(),
            Self::Denied { .. } => PERMISSION_DENIED,
        }
    }
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Link(error) => write!(f, "{error}"),
            Self::Denied { operation } => {
                write!(
                    f,
                    "the caller may not {operation} a link: it needs the link manage right"
                )
            }
        }
    }
}

impl std::error::Error for OpError {}

impl From<Error> for OpError {
    fn from(error: Error) -> Self {
        Self::Link(error)
    }
}
