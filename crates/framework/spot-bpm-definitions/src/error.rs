//! Typed errors for the definition store: one enum with stable `bpm_definitions.*` codes.
//!
//! Callers match on [`DefinitionError::code`], never on the message. No variant carries a
//! definition text, a note, a publisher or a name a caller supplied: a refused name may have been
//! a path or a secret, so the message names the field only. Every code has an entry in
//! `spot-errors`' `bpm_definitions.toml`, all with `class = "none"` because these errors never
//! ride an `AppError`; the host that refuses a write records the refusal.

/// Result of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, DefinitionError>;

/// The stable codes, as constants for hosts that match on them.
pub mod codes {
    /// Another publish won the version number the caller expected to create.
    pub const VERSION_CONFLICT: &str = "bpm_definitions.version_conflict";
    /// The named version does not exist.
    pub const NOT_FOUND: &str = "bpm_definitions.not_found";
    /// A definition, a publisher or a note is over its size cap.
    pub const TOO_LARGE: &str = "bpm_definitions.too_large";
    /// A process name breaks the model's name grammar.
    pub const NAME_INVALID: &str = "bpm_definitions.name_invalid";
    /// The text does not parse as a process of the named name, or a field is empty.
    pub const INPUT_INVALID: &str = "bpm_definitions.input_invalid";
    /// The backend failed, or a stored row cannot be read back.
    pub const STORE_FAILED: &str = "bpm_definitions.store_failed";
}

/// One reason a store refused or failed an operation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefinitionError {
    /// The publish expected a latest version that is no longer the latest.
    #[error(
        "publish of `{name}` expected latest version {expected:?}, but the latest is {found:?}"
    )]
    VersionConflict {
        /// The process name.
        name: String,
        /// The latest version the caller expected (`None`: no version yet).
        expected: Option<u32>,
        /// The latest version the store holds (`None`: no version).
        found: Option<u32>,
    },
    /// The version does not exist for this tenant.
    #[error("process `{name}` has no version {version}")]
    NotFound {
        /// The process name.
        name: String,
        /// The missing version number.
        version: u32,
    },
    /// A field is over its size cap.
    #[error("{field} is {len} bytes, over the cap of {max}")]
    TooLarge {
        /// `text`, `publisher` or `note`.
        field: &'static str,
        /// The size the caller sent.
        len: usize,
        /// The cap.
        max: usize,
    },
    /// The process name breaks the grammar `[a-z][a-z0-9_]{0,63}`. The name is not echoed.
    #[error("the process name breaks the name grammar")]
    NameInvalid,
    /// A field is empty or the text is not a process of the named name. Nothing of the text is
    /// echoed.
    #[error("the {field} is not acceptable")]
    InputInvalid {
        /// `text`, `publisher` or `note`.
        field: &'static str,
    },
    /// The backend failed or a stored row is corrupt. It carries the store name and the SQLSTATE
    /// class (or `pool`, `corrupt`) only, never row content.
    #[error("definition store `{store}` failed ({class})")]
    StoreFailed {
        /// The store's name.
        store: &'static str,
        /// The SQLSTATE class, or a short fixed word.
        class: String,
    },
}

impl DefinitionError {
    /// The stable code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::VersionConflict { .. } => codes::VERSION_CONFLICT,
            Self::NotFound { .. } => codes::NOT_FOUND,
            Self::TooLarge { .. } => codes::TOO_LARGE,
            Self::NameInvalid => codes::NAME_INVALID,
            Self::InputInvalid { .. } => codes::INPUT_INVALID,
            Self::StoreFailed { .. } => codes::STORE_FAILED,
        }
    }
}
