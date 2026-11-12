//! The dispatcher's and the executor's error vocabulary.
//!
//! Neither carries record content, a key or a personal value: the executor's failure is a
//! class plus a short snake_case reason code the console can translate.

use thiserror::Error;

/// The operation id is not one of the journal's.
pub const UNKNOWN_OPERATION: &str = "journal_ops.unknown_operation";
/// The invocation does not parse into the operation's typed arguments.
pub const BAD_ARGUMENTS: &str = "journal_ops.bad_arguments";
/// A result could not be turned into JSON.
pub const SERIALISE: &str = "journal_ops.serialise";
/// The caller's permission check failed (or could not decide).
pub const DENIED: &str = "journal_ops.denied";
/// The thing named does not exist.
pub const NOT_FOUND: &str = "journal_ops.not_found";
/// The state does not allow the operation now.
pub const CONFLICT: &str = "journal_ops.conflict";
/// A dependency is unreachable.
pub const UNAVAILABLE: &str = "journal_ops.unavailable";
/// The operation started and failed.
pub const FAILED: &str = "journal_ops.failed";

/// Why an executor refused or failed an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecKind {
    /// The caller's permission check failed (fail closed: an executor that cannot decide
    /// reports this).
    Denied,
    /// The thing named does not exist (a snapshot, a grant, a subject).
    NotFound,
    /// The state does not allow it now (a rebuild already running, a plan hash that no
    /// longer matches).
    Conflict,
    /// A dependency (the object store, the key provider) is unreachable.
    Unavailable,
    /// The operation started and failed.
    Failed,
}

/// An executor's failure: a class and a reason code, nothing else.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("journal operation {kind:?}: {reason}")]
pub struct ExecError {
    /// The class of failure.
    pub kind: ExecKind,
    /// A short code of `[a-z0-9_]`, at most 64 characters.
    pub reason: String,
}

impl ExecKind {
    /// The stable, machine-readable code for this class of failure.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Denied => DENIED,
            Self::NotFound => NOT_FOUND,
            Self::Conflict => CONFLICT,
            Self::Unavailable => UNAVAILABLE,
            Self::Failed => FAILED,
        }
    }
}

impl ExecError {
    /// The stable code of this failure's class; the reason stays a separate field.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// An executor failure; `reason` is reduced to `[a-z0-9_]` and 64 characters. It is a
    /// code, never a message: callers pass constants, and this keeps stray punctuation out.
    #[must_use]
    pub fn new(kind: ExecKind, reason: &str) -> Self {
        let reason: String = reason
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() || c.is_ascii_digit() {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        Self { kind, reason }
    }
}

/// Why [`crate::dispatch`] did not produce a result.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum OpError {
    /// The operation id is not one of the journal's.
    #[error("`{op}` is not a journal operation")]
    UnknownOperation {
        /// The id as received.
        op: String,
    },
    /// The invocation does not parse into the operation's typed arguments.
    #[error("`{op}`: parameter `{param}`: {reason}")]
    BadArguments {
        /// The operation.
        op: String,
        /// The parameter at fault (empty when the whole set is).
        param: String,
        /// A short, content-free reason.
        reason: &'static str,
    },
    /// The executor refused or failed.
    #[error(transparent)]
    Exec(#[from] ExecError),
    /// A result could not be turned into JSON (a programming mistake in a result type).
    #[error("`{op}`: the result could not be serialised")]
    Serialise {
        /// The operation.
        op: String,
    },
}

impl OpError {
    /// The stable, machine-readable code for this error. An executor failure reports its
    /// class's code; no code embeds an operation id, a parameter or a reason.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownOperation { .. } => UNKNOWN_OPERATION,
            Self::BadArguments { .. } => BAD_ARGUMENTS,
            Self::Exec(e) => e.code(),
            Self::Serialise { .. } => SERIALISE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_reasons_are_reduced_to_codes() {
        let e = ExecError::new(ExecKind::Failed, "Key for alice@example.com missing");
        assert!(
            e.reason
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
        assert!(!e.reason.contains('@'));
        let long = ExecError::new(ExecKind::Failed, &"a".repeat(500));
        assert_eq!(long.reason.len(), 64);
    }
}
