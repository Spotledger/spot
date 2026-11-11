//! Errors with stable `script.*` codes, and their mapping onto the app API's `AppError`.

use spot_app_api::error::FieldPath;
use spot_app_api::{AppError, ErrorClass};
use thiserror::Error;

/// A position in the original source, from a source map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePosition {
    /// The original file name, as the source map names it.
    pub source: String,
    /// The line, counted from 1.
    pub line: u32,
    /// The column, counted from 1.
    pub column: u32,
}

/// One field a script reports as invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldProblem {
    /// The field path, for example `customer` or `items[2].qty`.
    pub field: String,
    /// The error code of the problem.
    pub code: String,
    /// The message for the user.
    pub message: String,
}

/// Why a script run failed. Every variant has a stable [`ScriptError::code`]; none of them
/// affects the engine or any other execution.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ScriptError {
    /// The script used more memory than its limit.
    #[error("script.memory_limit: the script used more than {limit} bytes of memory")]
    MemoryLimit {
        /// The limit, in bytes.
        limit: usize,
    },
    /// The script ran past its wall-clock deadline.
    #[error("script.timeout: the script did not finish within {millis} ms")]
    Timeout {
        /// The limit, in milliseconds.
        millis: u128,
    },
    /// The script used up its instruction budget.
    #[error(
        "script.out_of_budget: the script used its budget of about {instructions} instructions"
    )]
    OutOfBudget {
        /// The budget, in (approximate) instructions.
        instructions: u64,
    },
    /// The script overflowed its stack.
    #[error("script.stack_overflow: the script's stack overflowed")]
    StackOverflow,
    /// The script made more host calls than its limit.
    #[error("script.too_many_host_calls: more than {limit} host calls")]
    TooManyHostCalls {
        /// The limit.
        limit: u32,
    },
    /// The result is larger than the output limit.
    #[error("script.output_too_large: the result is larger than {limit} bytes")]
    OutputTooLarge {
        /// The limit, in bytes.
        limit: usize,
    },
    /// The source does not compile.
    #[error("script.compile: {0}")]
    Compile(String),
    /// The script has no such exported function.
    #[error("script.missing_entry: the script exports no function `{0}`")]
    MissingEntry(String),
    /// The arguments are not usable (for example not JSON-serialisable).
    #[error("script.bad_arguments: {0}")]
    BadArguments(String),
    /// A host function panicked. The execution was stopped; the engine is unaffected.
    #[error("script.host_panic: the host function `{0}` panicked")]
    HostPanic(String),
    /// The script threw an exception it did not handle.
    #[error("script.thrown: {name}: {message}")]
    Thrown {
        /// The error's name (`Error`, `TypeError`, `HostError`, ...).
        name: String,
        /// The error's message.
        message: String,
        /// The script's stack trace, if it has one.
        stack: Option<String>,
        /// The position of the throw in the original source, if a source map was supplied.
        position: Option<SourcePosition>,
    },
    /// The script threw a typed validation error: an object with a `code`, optionally a
    /// `message` and `fields`. See [`ScriptError::into_app_error`].
    #[error("script.validation: {code}: {message}")]
    Validation {
        /// The script's own error code, dotted lower-case (`selling.qty_not_positive`).
        code: String,
        /// The message for the user.
        message: String,
        /// The invalid fields, with their own codes and messages.
        fields: Vec<FieldProblem>,
    },
    /// Something inside the engine failed that no script caused.
    #[error("script.internal: {0}")]
    Internal(String),
}

impl ScriptError {
    /// The stable machine-readable code, also the prefix of the message. A script's own
    /// validation error reports its own code through [`ScriptError::into_app_error`]; here it is
    /// `script.validation`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MemoryLimit { .. } => "script.memory_limit",
            Self::Timeout { .. } => "script.timeout",
            Self::OutOfBudget { .. } => "script.out_of_budget",
            Self::StackOverflow => "script.stack_overflow",
            Self::TooManyHostCalls { .. } => "script.too_many_host_calls",
            Self::OutputTooLarge { .. } => "script.output_too_large",
            Self::Compile(_) => "script.compile",
            Self::MissingEntry(_) => "script.missing_entry",
            Self::BadArguments(_) => "script.bad_arguments",
            Self::HostPanic(_) => "script.host_panic",
            Self::Thrown { .. } => "script.thrown",
            Self::Validation { .. } => "script.validation",
            Self::Internal(_) => "script.internal",
        }
    }

    /// The `AppError` this failure surfaces as.
    ///
    /// - A script's **own** validation error ([`ScriptError::Validation`]) becomes a
    ///   `Validation`-class error that keeps the script's code and message. With no fields it is
    ///   `AppError::new(Validation, code, message)`; with fields it is one `AppError::field` per
    ///   the first field plus `with_field` for the rest, each with the field's own code, and the
    ///   top-level code stays `document.validation_failed` (the registry code of that class).
    /// - Everything else is a fault of the script or the engine, not the user's input: class
    ///   `Internal` with the `script.*` code and a message that carries no script source.
    pub fn into_app_error(self) -> AppError {
        match self {
            Self::Validation {
                code,
                message,
                fields,
            } => match fields.split_first() {
                None => AppError::new(ErrorClass::Validation, &code, message),
                Some((first, rest)) => {
                    let mut error =
                        AppError::field(path(&first.field), &first.code, first.message.clone());
                    for f in rest {
                        error = error.with_field(path(&f.field), &f.code, f.message.clone());
                    }
                    error.message = message;
                    error
                }
            },
            other => AppError::new(ErrorClass::Internal, other.code(), other.to_string()),
        }
    }
}

/// A field path from the script's text; a text that is not a path is kept as a plain field name
/// so a script's mistake never panics the host.
fn path(text: &str) -> FieldPath {
    FieldPath::parse(text).unwrap_or_else(|| FieldPath::field(text))
}

/// A failure a host function reports to the script (as a thrown `HostError`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {message}")]
pub struct HostError {
    /// A short machine-readable code the script can test (`not_found`, `denied`).
    pub code: String,
    /// What went wrong, for the script's author.
    pub message: String,
}

impl HostError {
    /// A host error with a code and a message.
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
        }
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, ScriptError>;
