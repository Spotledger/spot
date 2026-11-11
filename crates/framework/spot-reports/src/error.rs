//! The report engine's errors and their stable `report.*` codes.
//!
//! Compilation reports **every** problem at once ([`ReportError`] values, not one exception):
//! an author fixes a report definition in one pass. A code's meaning is stable; the catalogue
//! (`crates/framework/spot-errors/catalogue/report.toml`) carries the summary, remedy and
//! parameters. `report.not_found` and `report.bad_argument` are the contract's runtime codes;
//! the rest are load-time refusals that never reach a client.

use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;

/// The stable dotted codes this crate raises.
pub mod codes {
    /// No report with this name, or the caller's roles do not admit it (the contract's code).
    pub const NOT_FOUND: &str = "report.not_found";
    /// An argument is unknown or does not fit its parameter's type (the contract's code).
    pub const BAD_ARGUMENT: &str = "report.bad_argument";
    /// The definition is structurally or semantically wrong (bad name, duplicate, empty
    /// columns, unknown aggregate, a required parameter with a default, a limit above the
    /// maximum, an empty role list).
    pub const INVALID_DEFINITION: &str = "report.invalid_definition";
    /// The report's base DocType (or a joined one) is not in the loading site's metadata.
    pub const BASE_MISSING: &str = "report.base_missing";
    /// A column, group-by, order-by or computed field names a field the DocType does not have.
    pub const UNKNOWN_FIELD: &str = "report.unknown_field";
    /// A filter names a parameter the report does not declare.
    pub const UNKNOWN_PARAMETER: &str = "report.unknown_parameter";
    /// A field path names an alias that is neither the base nor a declared join.
    pub const UNKNOWN_JOIN: &str = "report.unknown_join";
    /// Two things share a name: two reports in the registry, two parameters, two columns.
    pub const DUPLICATE: &str = "report.duplicate";
    /// A parameter's type does not fit the field it filters, or an aggregate's argument type.
    pub const BAD_TYPE: &str = "report.bad_type";
    /// A computed column's expression does not parse, reads a field that is not a column, or
    /// fails to evaluate to a value the column's type accepts.
    pub const EXPRESSION: &str = "report.expression";
    /// The compiled query exceeds an ADR-069 limit (depth, nodes, a list length, select
    /// columns).
    pub const TOO_LARGE: &str = "report.too_large";
    /// The report declares more joins than ADR-069's `MAX_JOINS`.
    pub const JOIN_LIMIT: &str = "report.join_limit";
    /// A field the caller may not report on (the field guard's code, catalogued in
    /// `permission.toml`).
    pub const FIELD_DENIED: &str = "permission.field_denied";
}

/// One report problem: its stable code, a plain-language message and, when it concerns one
/// thing, the path it names.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ReportError {
    code: &'static str,
    message: String,
    path: Option<String>,
}

impl ReportError {
    /// An error under `code`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        ReportError {
            code,
            message: message.into(),
            path: None,
        }
    }

    /// An error naming one path (`account.name`, `parameter \`company\``).
    pub fn at(code: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        ReportError {
            code,
            message: message.into(),
            path: Some(path.into()),
        }
    }

    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The message, never a value or SQL.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The path this error names, if any.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// The native API error. The contract's runtime codes map to their registry entries (404,
    /// 422, 403); a load-time code maps to an app-coded error, which never reaches a client.
    #[must_use]
    pub fn to_api_error(&self) -> ApiError {
        match self.code {
            codes::NOT_FOUND => ApiError::new(Code::ReportNotFound, self.message.clone()),
            codes::BAD_ARGUMENT => ApiError::new(Code::ReportBadArgument, self.message.clone()),
            codes::FIELD_DENIED => {
                let mut e = ApiError::new(Code::PermissionFieldDenied, self.message.clone());
                if let Some(path) = &self.path {
                    e = e.with_field(spot_http::api::service::FieldIssue::new(
                        path,
                        self.code,
                        self.message.clone(),
                    ));
                }
                e
            }
            _ => ApiError::app(self.code, self.message.clone()),
        }
    }
}

/// The result of a compile that may report many errors.
pub type ReportResult<T> = Result<T, Vec<ReportError>>;
