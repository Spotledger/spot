//! The one error type of the crate and its stable codes (`fieldguard.toml`).
//!
//! An error names **paths and codes only**: never a field value, and (for the reveal) never
//! whether a document exists.

use spot_app_api::{AppError, ErrorClass, FieldPath};

/// A column or query field the caller may not use: the existing `permission.field_denied`.
pub use spot_permissions::FIELD_DENIED;

/// A reveal was refused. The same error for a missing document, a forbidden one, a field
/// that cannot be revealed and a caller without the right (catalogued in `permission.toml`).
pub const REVEAL_DENIED: &str = "permission.reveal_denied";

/// A reveal needs the reason text the audit record keeps. Class `Validation`.
pub const REASON_REQUIRED: &str = "fieldguard.reason_required";

/// `reveal_bulk` is for a system actor with the bulk right only. Class `PermissionDenied`.
pub const BULK_REFUSED: &str = "fieldguard.reveal_bulk_refused";

/// A job, sync or migration touched a protected group its manifest does not declare. Class
/// `PermissionDenied`.
pub const UNDECLARED_ACCESS: &str = "fieldguard.undeclared_system_access";

/// The field access trace could not take a record and the group is `fail_closed`, so the
/// value was withheld. Class `Unavailable`.
pub const TRACE_UNAVAILABLE: &str = "fieldguard.trace_unavailable";

/// The audit queue could not take the reveal record, so nothing was revealed. Class
/// `Unavailable`.
pub const AUDIT_UNAVAILABLE: &str = "fieldguard.audit_unavailable";

/// The DocType has no protection facts: the guard cannot tell what is protected, so it
/// refuses to release anything. Class `UnknownDocType`.
pub const UNKNOWN_DOCTYPE: &str = "fieldguard.unknown_doctype";

/// A refusal from the guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardError {
    code: &'static str,
    class: ErrorClass,
    message: String,
    paths: Vec<String>,
}

impl GuardError {
    fn make(code: &'static str, class: ErrorClass, message: &str, paths: Vec<String>) -> Self {
        GuardError {
            code,
            class,
            message: message.to_owned(),
            paths,
        }
    }

    /// Columns or query fields the caller may not use; the error names every path.
    #[must_use]
    pub fn field_denied(paths: Vec<String>) -> Self {
        Self::make(
            FIELD_DENIED,
            ErrorClass::PermissionDenied,
            "The caller may not use these fields here.",
            paths,
        )
    }

    /// The one reveal refusal.
    #[must_use]
    pub fn reveal_denied() -> Self {
        Self::make(
            REVEAL_DENIED,
            ErrorClass::PermissionDenied,
            "The reveal was refused.",
            Vec::new(),
        )
    }

    /// A reveal without a reason.
    #[must_use]
    pub fn reason_required() -> Self {
        Self::make(
            REASON_REQUIRED,
            ErrorClass::Validation,
            "A reveal needs a reason.",
            Vec::new(),
        )
    }

    /// A bulk reveal by someone who is not a system actor with the bulk right.
    #[must_use]
    pub fn bulk_refused() -> Self {
        Self::make(
            BULK_REFUSED,
            ErrorClass::PermissionDenied,
            "Bulk reveal is for back-end services only.",
            Vec::new(),
        )
    }

    /// A system actor touching an undeclared protected group of `path`.
    #[must_use]
    pub fn undeclared_access(path: &str) -> Self {
        Self::make(
            UNDECLARED_ACCESS,
            ErrorClass::PermissionDenied,
            "The actor did not declare this protection group.",
            vec![path.to_owned()],
        )
    }

    /// A trace queue that is full while the group is `fail_closed`.
    #[must_use]
    pub fn trace_unavailable() -> Self {
        Self::make(
            TRACE_UNAVAILABLE,
            ErrorClass::Unavailable,
            "The field access trace is unavailable; the value was withheld.",
            Vec::new(),
        )
    }

    /// An audit queue that is full.
    #[must_use]
    pub fn audit_unavailable() -> Self {
        Self::make(
            AUDIT_UNAVAILABLE,
            ErrorClass::Unavailable,
            "The audit record could not be written; nothing was revealed.",
            Vec::new(),
        )
    }

    /// A DocType the protection source does not know.
    #[must_use]
    pub fn unknown_doctype(doctype: &str) -> Self {
        Self::make(
            UNKNOWN_DOCTYPE,
            ErrorClass::UnknownDocType,
            "The DocType has no protection facts.",
            vec![doctype.to_owned()],
        )
    }

    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The error class the code rides on.
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        self.class
    }

    /// The field or DocType paths the error names, in the order found.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// The app API error: the code with a field error per path.
    #[must_use]
    pub fn to_app_error(&self) -> AppError {
        let mut e = AppError::new(self.class, self.code, self.message.clone());
        if self.code == FIELD_DENIED {
            for p in &self.paths {
                e = e.with_field(FieldPath::field(p), self.code, self.message.clone());
            }
        }
        e
    }
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if !self.paths.is_empty() {
            write!(f, " ({})", self.paths.join(", "))?;
        }
        Ok(())
    }
}

impl std::error::Error for GuardError {}
