//! The one error type of the redactor, with stable machine-readable codes.
//!
//! Messages never contain secret material: a failed secret registration reports lengths,
//! never values, and a failed policy parse reports what the toml parser said about the file
//! (a policy must not hold secret values anyway; see [`crate::policy::RedactionPolicy`]).

use std::fmt;

/// Failure registering a known secret or parsing a [`crate::policy::RedactionPolicy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedactError {
    /// The registered secret is shorter than the minimum length a safe match needs.
    SecretTooShort {
        /// The length in bytes of the value that was offered.
        len: usize,
    },
    /// The label is not one to twenty characters of `a-z`, `0-9` or `_`.
    BadLabel,
    /// The policy text failed to parse as [`crate::policy::RedactionPolicy`].
    Policy(String),
}

impl RedactError {
    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::SecretTooShort { .. } => "redact.secret_too_short",
            Self::BadLabel => "redact.bad_label",
            Self::Policy(_) => "redact.policy",
        }
    }
}

impl fmt::Display for RedactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Length, never the value: the brief forbids logging registered secrets.
            Self::SecretTooShort { len } => {
                write!(
                    f,
                    "redact.secret_too_short: secret is {len} bytes; the minimum is 8"
                )
            }
            Self::BadLabel => write!(
                f,
                "redact.bad_label: labels are 1-20 characters of a-z, 0-9 and _"
            ),
            // The toml error text quotes the policy file, which must not hold secrets.
            Self::Policy(text) => write!(f, "redact.policy: {text}"),
        }
    }
}

impl std::error::Error for RedactError {}

#[cfg(test)]
mod tests {
    use super::RedactError;

    #[test]
    fn error_messages_never_hold_a_value() {
        let e = RedactError::SecretTooShort { len: 3 };
        let text = e.to_string();
        assert_eq!(e.code(), "redact.secret_too_short");
        assert!(text.contains("3 bytes"), "the message reports the length");
        assert!(
            !text.contains("hunter"),
            "the message never repeats a secret"
        );
    }
}
