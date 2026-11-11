//! Typed, explained errors (ADR-26) and their mapping onto the native API registry (ADR-055).
//!
//! Every error an app or the platform raises is an [`AppError`]: a stable dotted code, a short
//! plain-language message, an optional longer explanation, field errors with exact paths, and a
//! [`ErrorClass`] that fixes how the native API reports it. App codes (`selling.qty_not_positive`)
//! are free-form but dotted; the platform codes are the ones in the native API registry.

use std::fmt;

use serde_json::{Map, Value as Json};
use spot_storage_contract::{ErrorKind, StorageError};

/// Result of every app API operation.
pub type AppResult<T> = Result<T, AppError>;

/// How an error is reported. Each class has one native API registry code, which the HTTP layer
/// uses for the status (see [`AppError::api_code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// One or more fields are invalid: `document.validation_failed` (422). Also the class of
    /// every error an app raises with a field path.
    Validation,
    /// The document does not exist or the user may not see it: `document.not_found` (404).
    NotFound,
    /// The DocType does not exist: `meta.unknown_doctype` (404).
    UnknownDocType,
    /// The document changed since it was read: `document.stale_version` (412).
    StaleVersion,
    /// A unique value is already used: `document.duplicate` (409).
    Duplicate,
    /// Other documents link to this one: `document.linked` (409).
    Linked,
    /// The action is not allowed in the document's state: `document.invalid_state` (409).
    InvalidState,
    /// The user may not do this: `permission.denied` (403).
    PermissionDenied,
    /// A method, function or seam does not exist: `method.not_found` (404).
    MethodNotFound,
    /// Temporarily unavailable; retry later with the same command id: `server.unavailable` (503).
    Unavailable,
    /// A programming error in an app or the platform, for example a phase violation:
    /// `http.internal` (500). The code and details are logged, never shown to end users.
    Internal,
}

impl ErrorClass {
    /// The native API registry code this class is reported as.
    pub fn api_code(self) -> &'static str {
        match self {
            ErrorClass::Validation => "document.validation_failed",
            ErrorClass::NotFound => "document.not_found",
            ErrorClass::UnknownDocType => "meta.unknown_doctype",
            ErrorClass::StaleVersion => "document.stale_version",
            ErrorClass::Duplicate => "document.duplicate",
            ErrorClass::Linked => "document.linked",
            ErrorClass::InvalidState => "document.invalid_state",
            ErrorClass::PermissionDenied => "permission.denied",
            ErrorClass::MethodNotFound => "method.not_found",
            ErrorClass::Unavailable => "server.unavailable",
            ErrorClass::Internal => "http.internal",
        }
    }

    /// Whether the same request may succeed later unchanged.
    pub fn retryable(self) -> bool {
        matches!(self, ErrorClass::Unavailable)
    }
}

/// A field, or a field of one child-table row: `customer` or `items[2].qty` (idx from 1). The
/// same form as the native API's `FieldPath`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldPath(String);

impl FieldPath {
    /// A top-level field.
    pub fn field(name: &str) -> Self {
        Self(name.to_owned())
    }

    /// A field of the child row at `idx` (from 1) of `table`.
    pub fn row(table: &str, idx: u32, field: &str) -> Self {
        Self(format!("{table}[{idx}].{field}"))
    }

    /// Parses and validates a path.
    pub fn parse(s: &str) -> Option<Self> {
        let ident = |p: &str| {
            let mut b = p.bytes();
            matches!(b.next(), Some(c) if c.is_ascii_lowercase())
                && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        };
        let ok = match s.split_once('[') {
            None => ident(s),
            Some((table, rest)) => match rest.split_once("].") {
                Some((idx, field)) => {
                    ident(table)
                        && ident(field)
                        && !idx.starts_with('0')
                        && !idx.is_empty()
                        && idx.bytes().all(|b| b.is_ascii_digit())
                }
                None => false,
            },
        };
        ok.then(|| Self(s.to_owned()))
    }

    /// The path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One invalid field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    /// Which field.
    pub field: FieldPath,
    /// Stable dotted code, for example `selling.qty_not_positive` or `document.required`.
    pub code: String,
    /// Plain-language message.
    pub message: String,
}

/// A typed, explained error: one pointer wide, so `AppResult` stays cheap to return on every
/// path (clippy `result_large_err`). Its fields are read through `Deref` (`e.code`, `e.fields`)
/// and set through the builder methods.
#[derive(Clone, Debug, PartialEq)]
pub struct AppError(Box<ErrorBody>);

impl std::ops::Deref for AppError {
    type Target = ErrorBody;
    fn deref(&self) -> &ErrorBody {
        &self.0
    }
}

impl std::ops::DerefMut for AppError {
    fn deref_mut(&mut self) -> &mut ErrorBody {
        &mut self.0
    }
}

/// The contents of an [`AppError`].
#[derive(Clone, Debug, PartialEq)]
pub struct ErrorBody {
    /// How it is reported.
    pub class: ErrorClass,
    /// Stable dotted code. For platform errors, the registry code; for app errors, the app's
    /// own code (the class then decides the reported registry code).
    pub code: String,
    /// Short plain-language message.
    pub message: String,
    /// Longer offline help (ADR-26, WP-PRD-13).
    pub explanation: Option<String>,
    /// Field errors, in the order found.
    pub fields: Vec<FieldError>,
    /// Code-specific data, for example `current_version` for a stale version.
    pub details: Map<String, Json>,
}

impl AppError {
    /// An error of `class` with its own code and message.
    pub fn new(class: ErrorClass, code: &str, message: impl Into<String>) -> Self {
        debug_assert!(is_code(code), "error codes are dotted lower-case: {code}");
        Self(Box::new(ErrorBody {
            class,
            code: code.to_owned(),
            message: message.into(),
            explanation: None,
            fields: Vec::new(),
            details: Map::new(),
        }))
    }

    /// The error's contents, by value.
    pub fn into_body(self) -> ErrorBody {
        *self.0
    }

    /// A validation error for one field; add more with [`AppError::with_field`].
    pub fn field(path: FieldPath, code: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        let mut e = Self::new(
            ErrorClass::Validation,
            "document.validation_failed",
            message.clone(),
        );
        e.fields.push(FieldError {
            field: path,
            code: code.to_owned(),
            message,
        });
        e
    }

    /// Adds a field error, keeping the class `Validation`.
    pub fn with_field(mut self, path: FieldPath, code: &str, message: impl Into<String>) -> Self {
        self.fields.push(FieldError {
            field: path,
            code: code.to_owned(),
            message: message.into(),
        });
        self
    }

    /// Adds the offline explanation.
    pub fn explained(mut self, explanation: impl Into<String>) -> Self {
        self.explanation = Some(explanation.into());
        self
    }

    /// Adds one detail.
    pub fn detail(mut self, key: &str, value: impl Into<Json>) -> Self {
        self.details.insert(key.to_owned(), value.into());
        self
    }

    /// A phase violation: a hook did something its phase does not allow (ADR-51). Always a
    /// programming error, so it is `Internal`.
    pub fn phase_violation(what: &str, phase: &str) -> Self {
        Self::new(
            ErrorClass::Internal,
            "platform.phase_violation",
            format!("{what} is not allowed in the {phase} phase"),
        )
        .explained("Lifecycle hooks may only do what their phase allows (ADR-51): derive in before_*, check in validate_*, change other documents in after_*, and call outside systems only in *_committed.")
    }

    /// The code the native API reports: the class's registry code.
    pub fn api_code(&self) -> &'static str {
        self.class.api_code()
    }

    /// Whether an unchanged retry may succeed.
    pub fn retryable(&self) -> bool {
        self.class.retryable()
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

/// Whether `code` is a dotted lower-case code (`a.b`, `a.b_c.d`).
pub fn is_code(code: &str) -> bool {
    let parts: Vec<&str> = code.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|p| {
            let mut b = p.bytes();
            matches!(b.next(), Some(c) if c.is_ascii_lowercase())
                && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        })
}

/// What the host was doing when storage failed; decides how a reference violation is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageOp {
    /// Creating or updating a document.
    Write,
    /// Deleting a document.
    Delete,
    /// Anything else (loads, queries, commit).
    Other,
}

/// Maps a storage error onto an app error. Hosts use this one table so every host reports
/// storage failures the same way. Serialization failures and deadlocks are retried by the host
/// first; this mapping is for when retries are exhausted.
pub fn map_storage_error(e: &StorageError, during: StorageOp) -> AppError {
    use ErrorClass as C;
    let (class, code, message): (ErrorClass, &str, &str) = match e.kind {
        ErrorKind::NotFound => (
            C::NotFound,
            "document.not_found",
            "The document does not exist.",
        ),
        ErrorKind::StaleVersion => (
            C::StaleVersion,
            "document.stale_version",
            "The document was changed by someone else.",
        ),
        ErrorKind::UniqueViolation => (
            C::Duplicate,
            "document.duplicate",
            "Another document already has this value.",
        ),
        ErrorKind::AlreadyExists => (
            C::Duplicate,
            "document.duplicate",
            "A document or row with this id already exists.",
        ),
        ErrorKind::ReferenceViolation if during == StorageOp::Delete => (
            C::Linked,
            "document.linked",
            "Other documents link to this one.",
        ),
        ErrorKind::ReferenceViolation => (
            C::Validation,
            "document.invalid_link",
            "A link points at a document that does not exist.",
        ),
        ErrorKind::InvalidValue => (
            C::Validation,
            "document.invalid_value",
            "A value does not fit its field.",
        ),
        ErrorKind::UnknownField => (
            C::Validation,
            "document.unknown_field",
            "The DocType has no such field.",
        ),
        ErrorKind::UnknownEntity => (
            C::UnknownDocType,
            "meta.unknown_doctype",
            "No DocType with this name.",
        ),
        ErrorKind::SerializationFailure | ErrorKind::Deadlock => (
            C::Unavailable,
            "server.conflict_retry_exhausted",
            "Concurrent changes kept conflicting; try again.",
        ),
        ErrorKind::LockTimeout | ErrorKind::LockNotAvailable | ErrorKind::StatementTimeout => (
            C::Unavailable,
            "server.unavailable",
            "The document is busy; try again.",
        ),
        ErrorKind::Unavailable | ErrorKind::CommitOutcomeUnknown => (
            C::Unavailable,
            "server.unavailable",
            "The service is temporarily unavailable; retry with the same request id.",
        ),
        _ => (C::Internal, "http.internal", "An unexpected storage error."),
    };
    let mut err = AppError::new(class, code, message).detail("storage_code", e.kind.code());
    if let (C::Validation, Some(field)) = (class, e.field.as_ref()) {
        err = AppError::field(FieldPath::field(field.as_str()), code, message)
            .detail("storage_code", e.kind.code());
    }
    err
}
