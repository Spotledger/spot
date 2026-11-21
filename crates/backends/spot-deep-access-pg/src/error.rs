//! Fixed-sentence errors with stable codes; never a row value, key or nonce.

/// The schema could not be created.
pub const MIGRATION_FAILED: &str = "deep_access_pg.migration_failed";
/// A statement failed or no connection was available.
pub const STORE_FAILED: &str = "deep_access_pg.store_failed";

/// Why a store call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// `deep_access_pg.migration_failed`.
    Migration(String),
    /// `deep_access_pg.store_failed`: the database could not answer; the caller fails closed.
    Failed,
    /// An input was refused before the database was asked (over-long text, bad length).
    Invalid(&'static str),
}

impl StoreError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Migration(_) => MIGRATION_FAILED,
            Self::Failed | Self::Invalid(_) => STORE_FAILED,
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Migration(why) => write!(f, "the deep access tables could not be created: {why}"),
            Self::Failed => f.write_str("the deep access store could not answer"),
            Self::Invalid(why) => write!(f, "the deep access store refused the input: {why}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// A database failure as a fixed sentence plus the SQLSTATE.
pub(crate) fn describe(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("database error {}", db.code().code()),
        None => "the database could not be reached or answered unexpectedly".to_owned(),
    }
}

pub(crate) fn failed(_: &tokio_postgres::Error) -> StoreError {
    StoreError::Failed
}

pub(crate) fn pool_failed(_: &deadpool_postgres::PoolError) -> StoreError {
    StoreError::Failed
}
