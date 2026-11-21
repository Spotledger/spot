//! The store's fixed-text error. It never carries a driver message, SQLSTATE, site or fingerprint.

use std::fmt;

/// The pool or the database failed on a read or a write.
pub const UNAVAILABLE: &str = "node_role_pg.unavailable";
/// The node role table does not exist (SQLSTATE 42P01).
pub const TABLE_MISSING: &str = "node_role_pg.table_missing";

/// A failed read or write of the node role.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NodeRoleStoreError {
    code: &'static str,
}

impl NodeRoleStoreError {
    /// The stable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    pub(crate) fn unavailable() -> Self {
        Self { code: UNAVAILABLE }
    }

    pub(crate) fn table_missing() -> Self {
        Self {
            code: TABLE_MISSING,
        }
    }

    /// Classifies a driver error by SQLSTATE only; the text is dropped.
    pub(crate) fn from_driver(error: &tokio_postgres::Error) -> Self {
        match error.code() {
            Some(state) if *state == tokio_postgres::error::SqlState::UNDEFINED_TABLE => {
                Self::table_missing()
            }
            _ => Self::unavailable(),
        }
    }
}

impl fmt::Display for NodeRoleStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.code == TABLE_MISSING {
            write!(
                f,
                "{}: the node role table is missing; run spot migrate",
                self.code
            )
        } else {
            write!(f, "{}: the node role store is unavailable", self.code)
        }
    }
}

impl fmt::Debug for NodeRoleStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeRoleStoreError({})", self.code)
    }
}

impl std::error::Error for NodeRoleStoreError {}
