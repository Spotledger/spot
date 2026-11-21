//! The kit's own errors: loading an app and declaring users. Every operation on a document
//! reports the engine's own `AppError` instead, untouched.

use std::fmt;

/// Stable codes (catalogue: `app_testkit.toml`).
pub mod codes {
    /// The app failed its checks; the message lists every problem.
    pub const APP_INVALID: &str = "app_testkit.app_invalid";
    /// A test named a user it never declared.
    pub const UNKNOWN_USER: &str = "app_testkit.unknown_user";
    /// The kit does not run this part of an app.
    pub const UNSUPPORTED: &str = "app_testkit.unsupported";
}

/// A failure of the kit itself. `problems` holds one line per problem (for an invalid app, the
/// app check's own lines, each starting with its code); it never holds a field value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestkitError {
    /// One of [`codes`].
    pub code: &'static str,
    /// A short sentence.
    pub message: String,
    /// Every problem, in a stable order.
    pub problems: Vec<String>,
}

impl TestkitError {
    pub(crate) fn invalid(message: &str, problems: Vec<String>) -> Self {
        Self {
            code: codes::APP_INVALID,
            message: message.to_owned(),
            problems,
        }
    }

    pub(crate) fn unknown_user(name: &str) -> Self {
        Self {
            code: codes::UNKNOWN_USER,
            message: format!(
                "the user `{name}` was not declared; declare it with harness.user(..)"
            ),
            problems: Vec::new(),
        }
    }

    pub(crate) fn unsupported(what: &str) -> Self {
        Self {
            code: codes::UNSUPPORTED,
            message: format!("the test kit does not run {what}"),
            problems: Vec::new(),
        }
    }
}

impl fmt::Display for TestkitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        for problem in &self.problems {
            write!(f, "\n  - {problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for TestkitError {}

/// The kit's result.
pub type Result<T> = std::result::Result<T, TestkitError>;
