//! Errors of the BI views. Every code is catalogued in `spot-errors/catalogue/bi.toml`.
//!
//! Messages name policy and catalogue objects (entity and field names the administrator wrote
//! or the catalogue holds). They never carry row values and never carry SQL text.

use std::fmt;

/// The policy text is not valid TOML, or it has a key this version does not know.
pub const POLICY_PARSE: &str = "bi.policy_parse";
/// A policy value is out of range or inconsistent (a version below 1, a bad name, a child table
/// configured as a view of its own, a title field that is not exposed).
pub const POLICY_INVALID: &str = "bi.policy_invalid";
/// The policy names an entity the catalogue does not have.
pub const UNKNOWN_ENTITY: &str = "bi.unknown_entity";
/// The policy names a field the entity does not have.
pub const UNKNOWN_COLUMN: &str = "bi.unknown_column";
/// The policy asks for a field that is protected, secret or a system audit internal.
pub const PROTECTED_COLUMN: &str = "bi.protected_column";
/// The policy asks for a field whose type has no BI representation (JSON, bytes).
pub const UNSUPPORTED_TYPE: &str = "bi.unsupported_type";
/// A name is in the platform's reserved namespace (`spot_` prefix).
pub const RESERVED_NAME: &str = "bi.reserved_name";
/// The posted-only filter cannot be applied: no usable `docstatus` field, or it is protected.
pub const POSTED_MARKER: &str = "bi.posted_marker";
/// A generated view name is longer than PostgreSQL's identifier limit.
pub const NAME_TOO_LONG: &str = "bi.name_too_long";
/// Two generated objects would have the same view name.
pub const NAME_COLLISION: &str = "bi.name_collision";
/// Applying the plan would remove or retype a column of a view that exists; a new version is needed.
pub const BREAKING_CHANGE: &str = "bi.breaking_change";
/// An object with a planned view name exists in the `bi` schema and is not a view.
pub const NOT_A_VIEW: &str = "bi.not_a_view";
/// The database refused a statement. The message carries the SQLSTATE, never the statement.
pub const DATABASE: &str = "bi.database";

/// One problem, with its stable code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BiError {
    code: &'static str,
    message: String,
}

impl BiError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The stable code, for example `bi.protected_column`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for BiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BiError {}

/// Every problem found in one pass, so an administrator fixes them together.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BiErrors(Vec<BiError>);

impl BiErrors {
    pub(crate) fn push(&mut self, code: &'static str, message: impl Into<String>) {
        self.0.push(BiError::new(code, message));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The problems, in the order they were found.
    #[must_use]
    pub fn errors(&self) -> &[BiError] {
        &self.0
    }

    /// The codes, in the order they were found.
    #[must_use]
    pub fn codes(&self) -> Vec<&'static str> {
        self.0.iter().map(BiError::code).collect()
    }
}

impl fmt::Display for BiErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lines: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        write!(f, "{}", lines.join("\n"))
    }
}

impl std::error::Error for BiErrors {}
