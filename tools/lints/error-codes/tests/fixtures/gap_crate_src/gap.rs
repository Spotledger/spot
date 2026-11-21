//! Gap fixture source: raises the catalogued codes plus one code the catalogue doesn't know.

use spot_app_api::{AppError, ErrorClass};

/// The gap: this code is missing from the fixture catalogue and the lint must name this file
/// and line.
pub fn gap_raise() -> AppError {
    AppError::new(ErrorClass::Validation, "doc.missing", "The gap the tests look for.")
}

/// Raises the catalogued code too, so the gap is the only finding.
pub fn mixed_raise() -> AppError {
    AppError::new(ErrorClass::Validation, "doc.thing", "The known code.")
}

// The wrapper idiom: callers raise through it.
pub fn internal(code: &str, message: impl Into<String>) -> AppError {
    let _ = message;
    AppError::new(ErrorClass::Internal, code, "wrapped")
}

/// Raises through the wrapper — also counts.
pub fn wrapped_raise() -> AppError {
    let msg = "the fixture wraps";
    internal("doc.thing", msg)
}

// Test-gated raises count in neither direction: a gated raise ships to no caller.
#[cfg(test)]
mod inner {
    use spot_app_api::AppError;

    pub fn test_only() -> AppError {
        AppError::new(spot_app_api::ErrorClass::Internal, "doc.test_only", "test-only raise")
    }
}

/// A code only named in a comment is ignored.
pub fn documented() {
    // doc.comment_only and any.doc_comment_style are never scanned.
    let _ = 0;
}