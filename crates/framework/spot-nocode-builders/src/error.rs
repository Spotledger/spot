//! The builder's two error shapes: an [`Issue`] a person reads (every problem at once, in
//! plain language, with a fix) and an [`OpError`] a single draft edit returns.
//!
//! An issue never names the storage query IR, a SQL statement or a value: it says what the
//! designer can do about it. The stable code is the machine's half; the underlying layer's
//! code (`report.*`, `bpm_model.*`) is kept in [`Issue::source`] so the host can trace a
//! refusal without showing it.

use serde::Serialize;

use crate::codes;

/// How bad an issue is: an error blocks publish, a warning does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Blocks publish; the draft is not ready.
    Error,
    /// Worth fixing; the draft still publishes.
    Warning,
}

/// One problem with a draft, as a designer reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Issue {
    /// The stable `nocode_builders.*` code.
    pub code: &'static str,
    /// Whether it blocks publish.
    pub severity: Severity,
    /// The element it is about (`columns[2]`, `tiles[0]`, `approvers`), when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A plain-language sentence: what is wrong, never a value and never the query IR.
    pub message: String,
    /// A plain-language sentence: what to do about it.
    pub remedy: String,
    /// The underlying layer's stable code (`report.*`, `bpm_model.*`), for the host's log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
}

impl Issue {
    /// An error about the whole draft.
    #[must_use]
    pub fn error(
        code: &'static str,
        message: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Issue {
            code,
            severity: Severity::Error,
            path: None,
            message: message.into(),
            remedy: remedy.into(),
            source: None,
        }
    }

    /// An error about one element.
    #[must_use]
    pub fn at(
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Issue {
            code,
            severity: Severity::Error,
            path: Some(path.into()),
            message: message.into(),
            remedy: remedy.into(),
            source: None,
        }
    }

    /// A warning about one element.
    #[must_use]
    pub fn warning(
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Issue {
            code,
            severity: Severity::Warning,
            path: Some(path.into()),
            message: message.into(),
            remedy: remedy.into(),
            source: None,
        }
    }

    /// The same issue carrying the underlying layer's code.
    #[must_use]
    pub fn from_source(mut self, source: &'static str) -> Self {
        self.source = Some(source);
        self
    }

    /// Whether this issue blocks publish.
    #[must_use]
    pub fn blocks(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Whether any issue blocks publish.
#[must_use]
pub fn is_blocked(issues: &[Issue]) -> bool {
    issues.iter().any(Issue::blocks)
}

impl From<Issue> for Vec<Issue> {
    fn from(issue: Issue) -> Self {
        vec![issue]
    }
}

/// Why one draft edit could not be applied. Every variant carries a stable
/// `nocode_builders.*` code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpError {
    /// The draft has no such column.
    #[error("`{field}` is not a column of this draft")]
    UnknownColumn {
        /// The field path named.
        field: String,
    },
    /// The draft has no such parameter.
    #[error("`{name}` is not a value this draft offers")]
    UnknownParameter {
        /// The parameter named.
        name: String,
    },
    /// The draft has no such link.
    #[error("`{alias}` is not a link of this draft")]
    UnknownJoin {
        /// The alias named.
        alias: String,
    },
    /// Two things share a name.
    #[error("`{name}` is already used")]
    Duplicate {
        /// The name that collided.
        name: String,
    },
    /// The dashboard has no such tile.
    #[error("`{tile}` is not a tile of this dashboard")]
    UnknownTile {
        /// The tile named.
        tile: String,
    },
    /// A name is not a valid name.
    #[error("`{name}` is not a valid name")]
    InvalidName {
        /// The rejected name.
        name: String,
    },
    /// A value does not fit its setting.
    #[error("{message}")]
    BadValue {
        /// What is wrong, in plain language.
        message: String,
    },
    /// A setting does not fit the kind of field it uses.
    #[error("{message}")]
    BadType {
        /// What is wrong, in plain language.
        message: String,
    },
    /// A tile does not fit the grid, or overlaps another tile.
    #[error("{message}")]
    BadLayout {
        /// What is wrong, in plain language.
        message: String,
    },
    /// The dashboard's refresh policy is not one the platform accepts.
    #[error("{message}")]
    BadRefresh {
        /// What is wrong, in plain language.
        message: String,
    },
    /// The op is not supported for the draft it names.
    #[error("{what}")]
    Unsupported {
        /// What is not supported.
        what: String,
    },
}

impl OpError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownColumn { .. } => codes::UNKNOWN_COLUMN,
            Self::UnknownParameter { .. } => codes::UNKNOWN_PARAMETER,
            Self::UnknownJoin { .. } => codes::UNKNOWN_JOIN,
            Self::Duplicate { .. } => codes::DUPLICATE,
            Self::UnknownTile { .. } => codes::UNKNOWN_COLUMN,
            Self::InvalidName { .. } => codes::BAD_NAME,
            Self::BadValue { .. } => codes::BAD_VALUE,
            Self::BadType { .. } => codes::BAD_TYPE,
            Self::BadLayout { .. } => codes::BAD_LAYOUT,
            Self::BadRefresh { .. } => codes::BAD_REFRESH,
            Self::Unsupported { .. } => codes::UNSUPPORTED_OP,
        }
    }

    /// The element path the error is about, when it names one.
    #[must_use]
    pub fn path(&self) -> Option<String> {
        match self {
            Self::UnknownColumn { field } => Some(field.clone()),
            Self::UnknownParameter { name } => Some(name.clone()),
            Self::UnknownJoin { alias } => Some(alias.clone()),
            Self::Duplicate { name } => Some(name.clone()),
            Self::UnknownTile { tile } => Some(tile.clone()),
            Self::InvalidName { name } => Some(name.clone()),
            Self::BadValue { .. }
            | Self::BadType { .. }
            | Self::BadLayout { .. }
            | Self::BadRefresh { .. }
            | Self::Unsupported { .. } => None,
        }
    }
}
