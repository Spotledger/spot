//! Migration errors and their stable codes.

use thiserror::Error;

/// Stable codes of this crate (`migrate.*`, catalogued in `migrate.toml`).
pub mod codes {
    /// A destructive or lossy change.
    pub const REFUSED: &str = "migrate.refused";
    /// The live schema still differs from the catalogue after `apply`.
    pub const DRIFT: &str = "migrate.drift";
    /// A table stayed locked past the lock timeout, retries included.
    pub const LOCK_TIMEOUT: &str = "migrate.lock_timeout";
    /// `apply` stopped early (test hook).
    pub const INTERRUPTED: &str = "migrate.interrupted";
    /// The database refused a statement.
    pub const DATABASE: &str = "migrate.database";
    /// A new required field with rows and no default to fill them with.
    pub const NO_BACKFILL_DEFAULT: &str = "migrate.no_backfill_default";
    /// A unique index whose existing rows already hold duplicate values.
    pub const DUPLICATE_VALUES: &str = "migrate.duplicate_values";
    /// The catalogue needs an extension the database does not have installed (ADR-087). The
    /// remedy is the audited `extension.install` operation, never a migration.
    pub const EXTENSION_UNAVAILABLE: &str = "migrate.extension_unavailable";
    /// An extension is present (or offered) at a version older than the catalogue needs.
    pub const EXTENSION_VERSION: &str = "migrate.extension_version";
    /// `extension.install` named an extension outside the closed platform list.
    pub const EXTENSION_NOT_ALLOWED: &str = "migrate.extension_not_allowed";
    /// The connecting role cannot run `CREATE EXTENSION` for this extension.
    pub const EXTENSION_PRIVILEGE_MISSING: &str = "migrate.extension_privilege_missing";
    /// The database changed between the `extension.install` dry run and the install.
    pub const EXTENSION_PLAN_STALE: &str = "migrate.extension_plan_stale";
    /// An extra table handed to the planner breaks the rules of the hook (WP-STO-12.T3).
    pub const EXTRA_TABLE_INVALID: &str = "migrate.extra_table_invalid";
}

/// One refused change: what, where and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The entity (table) concerned.
    pub entity: String,
    /// The field (column) concerned, when the change is about one.
    pub field: Option<String>,
    /// The stable code: [`codes::REFUSED`], [`codes::NO_BACKFILL_DEFAULT`],
    /// [`codes::EXTENSION_UNAVAILABLE`] or [`codes::EXTENSION_VERSION`].
    pub code: &'static str,
    /// The named reason, one sentence, no values.
    pub reason: String,
}

/// Why a migration call failed.
#[derive(Debug, Error)]
pub enum MigrateError {
    /// The plan holds refused changes; nothing was applied.
    #[error("{}: {}", codes::REFUSED, .0.iter().map(|r| r.reason.as_str()).collect::<Vec<_>>().join("; "))]
    Refused(Vec<Refusal>),
    /// The schema still differs after `apply`.
    #[error("{}: {what}", codes::DRIFT)]
    Drift {
        /// The first differences found.
        what: String,
    },
    /// Locked out past the retry bound.
    #[error("{}", codes::LOCK_TIMEOUT)]
    LockTimeout,
    /// Stopped by the test hook after step `step`.
    #[error("{}: stopped after step {step}", codes::INTERRUPTED)]
    Interrupted {
        /// The index of the last step that completed.
        step: usize,
    },
    /// The database refused a statement; the message carries the SQLSTATE, never the SQL.
    #[error("{}: {message}", codes::DATABASE)]
    Database {
        /// SQLSTATE and the server's primary message.
        message: String,
    },
    /// A unique index build found rows that already hold the same values; the names come from
    /// the plan, never from the server's detail (which carries row values).
    #[error(
        "{}: cannot create unique index {} on {} ({}): existing rows hold duplicate values; \
         remove or merge the duplicates and run the migration again",
        codes::DUPLICATE_VALUES,
        .index,
        .table,
        .columns.join(", ")
    )]
    DuplicateValues {
        /// The table the index is built on.
        table: String,
        /// The index whose build failed.
        index: String,
        /// The indexed columns, in order. Empty when the plan does not name the index.
        columns: Vec<String>,
    },
    /// `extension.install` was asked for an extension that is not on the closed list.
    #[error("{}: {extension}", codes::EXTENSION_NOT_ALLOWED)]
    ExtensionNotAllowed {
        /// The requested name when it is a plain identifier, else a fixed placeholder.
        extension: String,
    },
    /// The role cannot create the extension; nothing was attempted.
    #[error("{}: {extension}: {statement}", codes::EXTENSION_PRIVILEGE_MISSING)]
    ExtensionPrivilegeMissing {
        /// The extension.
        extension: String,
        /// The exact statement an administrator holding the privilege can run.
        statement: String,
    },
    /// The extension is not installed and the server does not offer it.
    #[error("{}: {extension}: {reason}", codes::EXTENSION_UNAVAILABLE)]
    ExtensionUnavailable {
        /// The extension.
        extension: String,
        /// One sentence, no values.
        reason: String,
    },
    /// The extension (installed or offered) is older than needed, or the request's version
    /// bound is malformed.
    #[error("{}: {extension}: {reason}", codes::EXTENSION_VERSION)]
    ExtensionVersion {
        /// The extension.
        extension: String,
        /// One sentence, no values.
        reason: String,
    },
    /// The install was asked to run against a plan the database no longer matches.
    #[error("{}: {extension}", codes::EXTENSION_PLAN_STALE)]
    ExtensionPlanStale {
        /// The extension.
        extension: String,
    },
}

impl MigrateError {
    /// The stable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Refused(_) => codes::REFUSED,
            Self::Drift { .. } => codes::DRIFT,
            Self::LockTimeout => codes::LOCK_TIMEOUT,
            Self::Interrupted { .. } => codes::INTERRUPTED,
            Self::Database { .. } => codes::DATABASE,
            Self::DuplicateValues { .. } => codes::DUPLICATE_VALUES,
            Self::ExtensionNotAllowed { .. } => codes::EXTENSION_NOT_ALLOWED,
            Self::ExtensionPrivilegeMissing { .. } => codes::EXTENSION_PRIVILEGE_MISSING,
            Self::ExtensionUnavailable { .. } => codes::EXTENSION_UNAVAILABLE,
            Self::ExtensionVersion { .. } => codes::EXTENSION_VERSION,
            Self::ExtensionPlanStale { .. } => codes::EXTENSION_PLAN_STALE,
        }
    }
}

pub(crate) fn db(e: &tokio_postgres::Error) -> MigrateError {
    let state = e.code().map_or("-----", |c| c.code());
    let message = e
        .as_db_error()
        .map_or_else(|| e.to_string(), |d| d.message().to_owned());
    MigrateError::Database {
        message: format!("{state} {message}"),
    }
}

pub(crate) fn storage(e: &spot_storage_contract::StorageError) -> MigrateError {
    MigrateError::Database {
        message: e.to_string(),
    }
}

/// Whether a database error is a lock timeout or a deadlock (retryable).
pub(crate) fn is_lock_error(e: &tokio_postgres::Error) -> bool {
    matches!(e.code().map(|c| c.code()), Some("55P03" | "40P01"))
}
