//! Turning database, pool and row failures into [`DefinitionError`]s that carry the store name and
//! the SQLSTATE class only: never a row, a text, a note or a statement parameter.

use spot_bpm_definitions::DefinitionError;

/// The store's name in every [`DefinitionError::StoreFailed`].
pub(crate) const STORE: &str = "pg";

/// The SQLSTATE class (first two characters) of a driver error, or `io` when the server sent no
/// SQLSTATE (a closed connection).
pub(crate) fn class_of(error: &tokio_postgres::Error) -> String {
    error
        .code()
        .map_or_else(|| "io".to_owned(), |c| c.code().chars().take(2).collect())
}

/// A statement failure, as a store failure.
pub(crate) fn pg(error: &tokio_postgres::Error) -> DefinitionError {
    DefinitionError::StoreFailed {
        store: STORE,
        class: class_of(error),
    }
}

/// No connection could be checked out.
pub(crate) fn pool() -> DefinitionError {
    DefinitionError::StoreFailed {
        store: STORE,
        class: "pool".to_owned(),
    }
}

/// A stored row that cannot be read back.
pub(crate) fn corrupt() -> DefinitionError {
    DefinitionError::StoreFailed {
        store: STORE,
        class: "corrupt".to_owned(),
    }
}
