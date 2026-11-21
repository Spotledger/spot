//! Non-fatal messages: information and warnings shown with a successful result (the native
//! API's `messages`, what Frappe calls msgprint). Errors are [`AppError`](crate::AppError).

use crate::error::{FieldPath, is_code};

/// How important a message is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Level {
    /// For information.
    Info,
    /// Something the user should check; the operation still succeeded.
    Warning,
}

/// One message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// Importance.
    pub level: Level,
    /// Stable dotted code, for example `selling.below_last_price`.
    pub code: String,
    /// Plain-language text.
    pub text: String,
    /// The field it is about, if any.
    pub field: Option<FieldPath>,
}

impl Message {
    /// An information message.
    pub fn info(code: &str, text: impl Into<String>) -> Self {
        Self::new(Level::Info, code, text)
    }

    /// A warning.
    pub fn warning(code: &str, text: impl Into<String>) -> Self {
        Self::new(Level::Warning, code, text)
    }

    fn new(level: Level, code: &str, text: impl Into<String>) -> Self {
        debug_assert!(is_code(code), "message codes are dotted lower-case: {code}");
        Self {
            level,
            code: code.to_owned(),
            text: text.into(),
            field: None,
        }
    }

    /// Attaches the message to a field.
    pub fn on(mut self, field: FieldPath) -> Self {
        self.field = Some(field);
        self
    }
}
