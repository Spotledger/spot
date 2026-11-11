//! Typed errors for `spot-customise`: one enum with stable `customise.*` codes, and the
//! [`Issue`] list a validation failure carries so **every** problem is reported at once.

use std::fmt;

use crate::schema::RefusedStep;

/// Result of every fallible operation of the service.
pub type CustomiseResult<T> = Result<T, CustomiseError>;

/// One problem found while validating a change, located by a path.
///
/// `path` is `<doctype>` for a DocType-level problem and `<doctype>.<field>` for a field-level
/// one, so a client can point at the exact cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    /// The stable code of the problem (`customise.name_collision`, `customise.metadata_invalid`, ...).
    pub code: &'static str,
    /// Where: `<doctype>` or `<doctype>.<field>`; empty when the change names neither.
    pub path: String,
    /// What is wrong, in a sentence. Never carries a secret or SQL.
    pub message: String,
}

impl Issue {
    /// An issue at `path`.
    #[must_use]
    pub fn new(code: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}: {}", self.code, self.message)
        } else {
            write!(f, "{} at `{}`: {}", self.code, self.path, self.message)
        }
    }
}

/// Why a customisation call failed.
///
/// `#[non_exhaustive]`: new variants may be added; callers match on [`CustomiseError::code`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CustomiseError {
    /// The caller does not hold the platform `Customise` right (or, for a module other than
    /// `custom`, is not a platform administrator).
    #[error("`{user}` may not customise this site")]
    Forbidden {
        /// The caller.
        user: String,
    },
    /// The change does not validate. Every problem is listed, none is dropped.
    #[error("the change is not valid: {}", join(.issues))]
    Invalid {
        /// All problems, in a stable order.
        issues: Vec<Issue>,
    },
    /// The schema plan holds a destructive or lossy step. Nothing was persisted or applied.
    #[error("the change would need a destructive schema step: {}", join_refused(.steps))]
    Destructive {
        /// The refused steps, named.
        steps: Vec<RefusedStep>,
    },
    /// The schema backend failed (planning or applying); the change stays `pending` for
    /// `recover()` unless it was rolled back.
    #[error("the schema step failed: {message}")]
    Schema {
        /// What the backend reported, without SQL.
        message: String,
    },
    /// The record store failed.
    #[error("the customisation records could not be read or written: {message}")]
    Store {
        /// What the store reported.
        message: String,
    },
    /// The new snapshot could not be built, adopted or published.
    #[error("the metadata swap failed: {message}")]
    Swap {
        /// What the live metadata layer reported (including its own code).
        message: String,
    },
    /// This node cannot build the snapshot another node announced. It refuses to serve
    /// metadata it cannot reproduce rather than guess (`metalive.divergence`).
    #[error("this node diverged from the announced metadata: {message}")]
    Divergence {
        /// The divergence report.
        message: String,
    },
    /// The named customisation does not exist.
    #[error("there is no {what} to change")]
    NotFound {
        /// What was looked for.
        what: String,
    },
    /// A fault hook stopped the pipeline at `step` (tests simulate a crash this way).
    #[error("the pipeline was interrupted after step `{step}`")]
    Interrupted {
        /// The step the hook stopped after.
        step: &'static str,
    },
}

impl CustomiseError {
    /// The stable machine-readable code. Every code is `customise.<snake_case>`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Forbidden { .. } => "customise.forbidden",
            Self::Invalid { .. } => "customise.invalid",
            Self::Destructive { .. } => "customise.destructive_change",
            Self::Schema { .. } => "customise.schema_failed",
            Self::Store { .. } => "customise.store",
            Self::Swap { .. } => "customise.swap_failed",
            Self::Divergence { .. } => "customise.divergence",
            Self::NotFound { .. } => "customise.not_found",
            Self::Interrupted { .. } => "customise.interrupted",
        }
    }

    /// The issues of an [`Invalid`](CustomiseError::Invalid) error, empty otherwise.
    #[must_use]
    pub fn issues(&self) -> &[Issue] {
        match self {
            Self::Invalid { issues } => issues,
            _ => &[],
        }
    }

    /// The issue codes of an [`Invalid`](CustomiseError::Invalid) error, in order.
    #[must_use]
    pub fn issue_codes(&self) -> Vec<&'static str> {
        self.issues().iter().map(|issue| issue.code).collect()
    }
}

fn join(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn join_refused(steps: &[RefusedStep]) -> String {
    steps
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Issue codes raised while validating a change. Kept together so the catalogue and the code
/// scan have one place to look.
pub mod codes {
    /// The change text is not a valid definition, field spec or property.
    pub const BAD_DEFINITION: &str = "customise.bad_definition";
    /// A custom DocType name collides with an app DocType or another custom DocType.
    pub const NAME_COLLISION: &str = "customise.name_collision";
    /// A custom DocType name is reserved by the platform.
    pub const RESERVED_NAME: &str = "customise.reserved_name";
    /// A module other than `custom` was asked for by someone who is not a platform administrator.
    pub const MODULE_NOT_ALLOWED: &str = "customise.module_not_allowed";
    /// A custom field collides with a standard or existing field of the DocType.
    pub const FIELD_COLLISION: &str = "customise.field_collision";
    /// The metadata registry refused the result (carries the registry's own message).
    pub const METADATA_INVALID: &str = "customise.metadata_invalid";
    /// The plan compiler refused the result (a cycle in defaults and fetches, a bad rule).
    pub const PLAN_INVALID: &str = "customise.plan_invalid";
    /// A mandatory custom field on a table that holds rows needs a default to fill them with.
    pub const NEEDS_DEFAULT: &str = "customise.needs_default";
    /// A site tried to loosen a security property (only tightening is allowed).
    pub const SECURITY_LOOSENED: &str = "customise.security_loosened";
    /// The field is still referred to by something else; the referrers are listed.
    pub const REFERENCED: &str = "customise.referenced";
}
