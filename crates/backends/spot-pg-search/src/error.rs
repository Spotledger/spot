//! The errors this crate answers with: the storage vocabulary, plus the one permission refusal a
//! search can produce itself.
//!
//! A projection write is an ordinary storage write, so every failure it can have is a
//! [`StorageError`] with a kind the contract already defines (`storage.invalid_schema` for a
//! declaration the projection cannot honour, `storage.unsupported` for an operator the database
//! cannot serve, and so on). The one thing storage does not know about is permission, so the
//! refusal of a field the caller may not read unmasked is its own variant carrying
//! [`FIELD_DENIED`] — the same code the document engine's field guard uses (ADR-070 section 12
//! rule 7, ADR-087 section 7.7 behaviour 2). A caller maps it into its own error type; the crate
//! never invents a second code for it.

use std::fmt;

use spot_storage_contract::{ErrorKind, Name, StorageError};

/// The code of a refusal to read, filter, sort or search on a field the caller may not read
/// unmasked. Catalogued as `permission.field_denied`; a search naming a protected field is
/// refused with it rather than silently narrowed (ADR-087 section 7.7).
pub const FIELD_DENIED: &str = "permission.field_denied";

/// A failure of the search projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    /// A storage failure with the contract's kind and explanation.
    Storage(StorageError),
    /// The search, or a facet it asks for, names a field the caller may not read unmasked.
    FieldDenied {
        /// The field that was refused.
        field: Name,
    },
}

impl SearchError {
    /// The refusal of `field`: `permission.field_denied`.
    #[must_use]
    pub fn field_denied(field: &Name) -> Self {
        SearchError::FieldDenied {
            field: field.clone(),
        }
    }

    /// A declaration the projection cannot honour, with a plain-language explanation.
    #[must_use]
    pub fn invalid_schema(message: impl Into<String>) -> Self {
        SearchError::Storage(StorageError::new(ErrorKind::InvalidSchema, message))
    }

    /// Something the database cannot serve (a missing extension at run time, for example).
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        SearchError::Storage(StorageError::new(ErrorKind::Unsupported, message))
    }

    /// A failure of the connection or the statement.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        SearchError::Storage(StorageError::new(ErrorKind::Internal, message))
    }

    /// The stable code: a `storage.*` kind, or [`FIELD_DENIED`].
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            SearchError::Storage(e) => e.kind.code(),
            SearchError::FieldDenied { .. } => FIELD_DENIED,
        }
    }

    /// What happened, in plain language.
    #[must_use]
    pub fn explanation(&self) -> String {
        match self {
            SearchError::Storage(e) => e.explanation.clone(),
            SearchError::FieldDenied { field } => format!(
                "`{field}` is not a field you may read, so it cannot be searched, faceted or sorted \
                 on; a refusal is reported instead of an empty result"
            ),
        }
    }

    /// The field named by a refusal, when the failure names one.
    #[must_use]
    pub fn field(&self) -> Option<&Name> {
        match self {
            SearchError::Storage(e) => e.field.as_ref(),
            SearchError::FieldDenied { field } => Some(field),
        }
    }
}

impl From<StorageError> for SearchError {
    fn from(error: StorageError) -> Self {
        SearchError::Storage(error)
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.explanation())
    }
}

impl std::error::Error for SearchError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> Name {
        Name::new(text).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_field_refusal_keeps_the_catalogued_code() {
        let error = SearchError::field_denied(&name("owner_note"));
        assert_eq!(error.code(), FIELD_DENIED);
        assert_eq!(error.field().map(Name::as_str), Some("owner_note"));
        assert!(error.explanation().contains("owner_note"));
        assert!(
            !error.explanation().to_lowercase().contains("rule "),
            "the explanation names no rule text"
        );
    }

    #[test]
    fn a_storage_failure_keeps_its_kind_and_code() {
        let error = SearchError::from(StorageError::new(ErrorKind::Unsupported, "no pg_trgm here"));
        assert_eq!(error.code(), "storage.unsupported");
        assert_eq!(error.explanation(), "no pg_trgm here");
    }

    #[test]
    fn a_schema_refusal_is_storage_invalid_schema() {
        assert_eq!(
            SearchError::invalid_schema("no").code(),
            ErrorKind::InvalidSchema.code()
        );
        assert_eq!(
            SearchError::unsupported("no").code(),
            ErrorKind::Unsupported.code()
        );
        assert_eq!(
            SearchError::internal("no").code(),
            ErrorKind::Internal.code()
        );
    }
}
