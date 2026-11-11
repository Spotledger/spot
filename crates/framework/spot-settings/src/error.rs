//! Typed errors with stable `settings.*` codes.
//!
//! Every code raised by this crate is catalogued in
//! `crates/framework/spot-errors/catalogue/settings.toml`; the repository's error-code scan
//! fails if one is missing and if a catalogued entry is never raised.

use std::fmt;

use thiserror::Error;

/// The stable kind of a settings problem.
///
/// Each kind maps to exactly one `settings.*` code through [`SettingsErrorKind::code`]; the
/// code, not the message, is the stable contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SettingsErrorKind {
    /// Two specs share a key.
    DuplicateKey,
    /// A default does not satisfy the setting's own type.
    DefaultInvalid,
    /// A non-secret setting has no default.
    DefaultMissing,
    /// A spec declares no scopes.
    ScopesEmpty,
    /// A spec's scopes are not in broad-to-narrow order.
    ScopesOrder,
    /// A secret setting declares a default.
    SecretWithDefault,
    /// `depends_on` names a key that is not registered.
    DependsOnUnknown,
    /// `depends_on` forms a cycle.
    DependsOnCycle,
    /// A deprecation points to a key that is not registered.
    DeprecatedUnknown,
    /// A key does not match the required dotted lower-case format.
    KeyFormat,
    /// A value was set at a scope the spec does not allow.
    ScopeNotAllowed,
    /// A read-only setting was written.
    ReadOnly,
    /// A value failed validation against its setting's type.
    ValidationFailed,
    /// A change set contains duplicate or otherwise invalid changes.
    ChangeSetInvalid,
    /// A key is not registered.
    UnknownSetting,
    /// A compiled-in catalogue file could not be parsed.
    CatalogueInvalid,
}

impl SettingsErrorKind {
    /// The stable `settings.*` code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            SettingsErrorKind::DuplicateKey => "settings.duplicate_key",
            SettingsErrorKind::DefaultInvalid => "settings.default_invalid",
            SettingsErrorKind::DefaultMissing => "settings.default_missing",
            SettingsErrorKind::ScopesEmpty => "settings.scopes_empty",
            SettingsErrorKind::ScopesOrder => "settings.scopes_order",
            SettingsErrorKind::SecretWithDefault => "settings.secret_with_default",
            SettingsErrorKind::DependsOnUnknown => "settings.depends_on_unknown",
            SettingsErrorKind::DependsOnCycle => "settings.depends_on_cycle",
            SettingsErrorKind::DeprecatedUnknown => "settings.deprecated_unknown",
            SettingsErrorKind::KeyFormat => "settings.key_format",
            SettingsErrorKind::ScopeNotAllowed => "settings.scope_not_allowed",
            SettingsErrorKind::ReadOnly => "settings.read_only",
            SettingsErrorKind::ValidationFailed => "settings.validation_failed",
            SettingsErrorKind::ChangeSetInvalid => "settings.change_set_invalid",
            SettingsErrorKind::UnknownSetting => "settings.unknown_setting",
            SettingsErrorKind::CatalogueInvalid => "settings.catalogue_invalid",
        }
    }
}

impl fmt::Display for SettingsErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// One settings problem: a stable [`SettingsErrorKind`] and a human-readable message.
///
/// Messages name the offending key or field and never echo a secret value or a raw SQL
/// string.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{kind}: {message}")]
pub struct SettingsError {
    /// The stable kind.
    pub kind: SettingsErrorKind,
    /// The human-readable message.
    pub message: String,
}

impl SettingsError {
    /// Creates a problem of `kind` with `message`.
    pub fn new(kind: SettingsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The stable `settings.*` code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

/// A collection of problems, reported together so one build or validation pass shows them all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsErrorReport {
    errors: Vec<SettingsError>,
}

impl SettingsErrorReport {
    /// An empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one problem.
    pub fn push(&mut self, error: SettingsError) {
        self.errors.push(error);
    }

    /// Adds every problem of `other`.
    pub fn extend(&mut self, other: SettingsErrorReport) {
        self.errors.extend(other.errors);
    }

    /// Whether the report holds no problems.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// The number of problems.
    #[must_use]
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Every problem, in the order it was found.
    #[must_use]
    pub fn errors(&self) -> &[SettingsError] {
        &self.errors
    }

    /// Whether any problem carries `code`.
    #[must_use]
    pub fn has_code(&self, code: &str) -> bool {
        self.errors.iter().any(|error| error.code() == code)
    }

    /// `Ok(())` when empty; `Err(self)` otherwise.
    ///
    /// # Errors
    ///
    /// Returns the report itself when it holds at least one problem.
    pub fn into_result(self) -> Result<(), Self> {
        if self.is_empty() { Ok(()) } else { Err(self) }
    }
}

impl fmt::Display for SettingsErrorReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index > 0 {
                writeln!(f)?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SettingsErrorReport {}

impl From<SettingsError> for SettingsErrorReport {
    fn from(error: SettingsError) -> Self {
        let mut report = SettingsErrorReport::new();
        report.push(error);
        report
    }
}
