//! Clean fixture source: raises exactly the codes the fixture catalogue knows.

use spot_app_api::{AppError, ErrorClass};

/// Raises the catalogued code through the constructor.
pub fn clean_raise() -> AppError {
    AppError::new(ErrorClass::Validation, "doc.thing", "The value is wrong.")
}

// The wrapper idiom: callers raise through it.
pub fn internal(code: &str, message: impl Into<String>) -> AppError {
    let _ = AppError::new(ErrorClass::Internal, code, message);
    AppError::new(ErrorClass::Internal, code, "wrapped")
}

/// Raises through the wrapper — also clean.
pub fn wrapped_raise() -> AppError {
    let msg = "the fixture wraps";
    internal("doc.thing", msg)
}

// A code only named in a comment is ignored: doc.comment_only
/// The doc comment names doc.doc_comment_only, which is not a raise either.
pub fn documented() {}