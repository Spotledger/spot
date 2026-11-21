//! The one error type, with stable `permquery.*` codes.

use thiserror::Error;

/// Why a permission query could not be built or a caller's query was refused.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PermQueryError {
    /// A filter, sort, group, aggregate or join key uses a field the principal may not read
    /// unmasked. The same error for a hidden, a masked and a missing field, so a refusal never
    /// tells whether the field exists.
    #[error("the field `{path}` cannot be used to filter, sort, group or aggregate")]
    FieldNotFilterable {
        /// `doctype.field`, as the caller wrote it.
        path: String,
    },
    /// The principal holds more shared documents than one filter can list.
    #[error("{count} shared documents are more than one filter can list")]
    TooManyShares {
        /// How many shares were found.
        count: usize,
    },
    /// The compiled filter would be larger than the budget that leaves room for the caller's
    /// own query (ADR-069).
    #[error("the permission filter would need {nodes} expression nodes")]
    FilterTooLarge {
        /// Nodes the filter would have.
        nodes: usize,
    },
    /// The caller's query is bigger than the checker will walk (ADR-069).
    #[error("the query is too large to check")]
    QueryTooLarge,
}

impl PermQueryError {
    /// The stable machine code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            PermQueryError::FieldNotFilterable { .. } => "permquery.field_not_filterable",
            PermQueryError::TooManyShares { .. } => "permquery.too_many_shares",
            PermQueryError::FilterTooLarge { .. } => "permquery.filter_too_large",
            PermQueryError::QueryTooLarge => "permquery.query_too_large",
        }
    }
}
