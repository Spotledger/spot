//! The `naming.*` error vocabulary.
//!
//! Every variant carries one stable code (see [`NamingError::code`]); the codes and their
//! summaries live in `crates/framework/spot-errors/catalogue/naming.toml`. A storage failure
//! is wrapped whole ([`NamingError::Storage`]) so the caller keeps its
//! [`ErrorKind`](spot_storage_contract::ErrorKind), except a duplicate name, which the host
//! reports as [`NamingError::Duplicate`].

use spot_storage_contract::StorageError;

/// A naming failure with a stable `naming.*` code.
#[derive(Debug, thiserror::Error)]
pub enum NamingError {
    /// The registry holds no naming plan for the DocType.
    #[error("the DocType `{doctype}` has no compiled naming plan")]
    UnknownDoctype {
        /// The DocType asked for.
        doctype: String,
    },
    /// A value is not a legal record name.
    #[error("`{value}` is not a legal record name: {reason}")]
    InvalidName {
        /// The rejected value.
        value: String,
        /// Why it is not legal.
        reason: &'static str,
    },
    /// A pattern reads a field the document has not set.
    #[error("the naming pattern of `{doctype}` reads field `{field}`, which is not set")]
    MissingField {
        /// The DocType.
        doctype: String,
        /// The field named by the pattern.
        field: String,
    },
    /// The generated name is already used by another document.
    #[error("the name `{name}` is already used by another `{doctype}` document")]
    Duplicate {
        /// The DocType.
        doctype: String,
        /// The name already in use.
        name: String,
    },
    /// A prompt-named DocType got no caller-supplied name.
    #[error("the DocType `{doctype}` is named by its caller, but no name was supplied")]
    PromptRequired {
        /// The DocType.
        doctype: String,
    },
    /// The compiled plan is not a naming strategy this crate can run.
    #[error("the naming plan of `{doctype}` is not a strategy the namer can run")]
    PatternInvalid {
        /// The DocType.
        doctype: String,
    },
    /// The derived series key is not a legal series key.
    #[error("the derived series key is not legal: {reason}")]
    SeriesKeyInvalid {
        /// Why storage refused the key.
        reason: String,
    },
    /// `set_next` would hand a number out again.
    #[error(
        "series `{series}` has already handed out numbers up to or past {next}; a number cannot be reused"
    )]
    SeriesBackwards {
        /// The series.
        series: String,
        /// The refused next number.
        next: u64,
    },
    /// A gapless-strict series refuses to skip numbers.
    #[error("the gapless-strict series `{series}` refuses to skip {count} numbers")]
    SkipRefused {
        /// The series.
        series: String,
        /// The skipped count.
        count: u32,
    },
    /// A gapless-strict series refuses a reserved block.
    #[error("the gapless-strict series `{series}` refuses a reserved block")]
    ReserveRefused {
        /// The series.
        series: String,
    },
    /// The gap policy does not fit the operation.
    #[error("{message}")]
    PolicyUnsupported {
        /// What the caller should do instead.
        message: String,
    },
    /// A reserved block was consumed past its end.
    #[error("the reserved block of {count} numbers is exhausted")]
    BlockExhausted {
        /// How many numbers the block held.
        count: u32,
    },
    /// The storage allocator failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl NamingError {
    /// The stable dotted code (`naming.*`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownDoctype { .. } => "naming.unknown_doctype",
            Self::InvalidName { .. } => "naming.invalid_name",
            Self::MissingField { .. } => "naming.missing_field",
            Self::Duplicate { .. } => "naming.duplicate",
            Self::PromptRequired { .. } => "naming.prompt_required",
            Self::PatternInvalid { .. } => "naming.pattern_invalid",
            Self::SeriesKeyInvalid { .. } => "naming.series_key_invalid",
            Self::SeriesBackwards { .. } => "naming.series_backwards",
            Self::SkipRefused { .. } => "naming.skip_refused",
            Self::ReserveRefused { .. } => "naming.reserve_refused",
            Self::PolicyUnsupported { .. } => "naming.policy_unsupported",
            Self::BlockExhausted { .. } => "naming.block_exhausted",
            Self::Storage(_) => "naming.storage",
        }
    }

    /// The duplicate-name error a host raises when a write fails with
    /// `storage.unique_violation` on the naming field.
    pub fn duplicate(doctype: &str, name: &str) -> Self {
        Self::Duplicate {
            doctype: doctype.to_owned(),
            name: name.to_owned(),
        }
    }
}
