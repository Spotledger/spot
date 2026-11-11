//! The one error type, with stable, machine-readable `perm.*` codes, reported **all at once**.

/// Something failed while loading or building the permission model.
///
/// Variants carry what went wrong in words; for logs, metrics and API responses use
/// [`PermError::code`]. Codes are stable (blueprint §4).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PermError {
    /// A TOML document failed to parse. The message names the file and the parse error.
    #[error("cannot read {at}: {detail}")]
    Parse {
        /// The file or section the text came from.
        at: String,
        /// What the parser objected to.
        detail: String,
    },

    /// A key a table does not want. Unknown keys are errors, so a typo is caught at load.
    #[error("{at}: unknown key `{key}`")]
    UnknownField {
        /// The file or section.
        at: String,
        /// The unexpected key.
        key: String,
    },

    /// A required key was missing, or an optional one had an empty right set or an empty
    /// `allowed` list.
    #[error("{at}: {detail}")]
    Missing {
        /// The file or section.
        at: String,
        /// What was missing.
        detail: String,
    },

    /// A value had the wrong shape: a right no [`crate::Right`] names, a permlevel out of
    /// range, a record id that is not canonical UUIDv7 text.
    #[error("{at}: {detail}")]
    ValueInvalid {
        /// The file or section.
        at: String,
        /// What was wrong.
        detail: String,
    },

    /// The model contradicts itself: a grant on an unknown DocType, a user permission naming
    /// an unknown DocType or a field that is not a link or select field.
    #[error("{at}: {detail}")]
    Inconsistent {
        /// The file or section.
        at: String,
        /// The contradiction.
        detail: String,
    },
}

impl PermError {
    /// The stable machine-readable code, for logs, API responses and metrics.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse { .. } => PARSE,
            Self::UnknownField { .. } => UNKNOWN_FIELD,
            Self::Missing { .. } => MISSING,
            Self::ValueInvalid { .. } => VALUE_INVALID,
            Self::Inconsistent { .. } => INCONSISTENT,
        }
    }

    /// The file or section the error names.
    #[must_use]
    pub fn place(&self) -> &str {
        match self {
            Self::Parse { at, .. }
            | Self::UnknownField { at, .. }
            | Self::Missing { at, .. }
            | Self::ValueInvalid { at, .. }
            | Self::Inconsistent { at, .. } => at,
        }
    }
}

/// A TOML or JSON document of model data failed to parse.
pub const PARSE: &str = "perm.parse_failed";
/// The document used a key the model does not accept.
pub const UNKNOWN_FIELD: &str = "perm.unknown_field";
/// A required key or value was missing (or an empty set where one is not allowed).
pub const MISSING: &str = "perm.missing";
/// A value had the wrong shape or content.
pub const VALUE_INVALID: &str = "perm.value_invalid";
/// The model contradicts known metadata or itself.
pub const INCONSISTENT: &str = "perm.inconsistent";

/// Every load error at once: a site fixing a grants file sees all its problems, not the first.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{} problem(s) loading the permission model", .0.len())]
pub struct PermErrors(pub Vec<PermError>);

impl PermErrors {
    /// The errors, in the order the loader found them.
    #[must_use]
    pub fn as_slice(&self) -> &[PermError] {
        &self.0
    }

    /// Whether nothing was wrong.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
