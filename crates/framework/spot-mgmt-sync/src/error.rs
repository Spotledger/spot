//! The dispatcher's and the executor's error vocabulary.
//!
//! Neither carries record content, a key, an enrolment code or a personal value: the
//! executor's failure is a class plus a short snake_case reason code the console can translate.

use thiserror::Error;

/// Why an executor refused or failed an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecKind {
    /// The caller's permission check failed (fail closed: an executor that cannot decide
    /// reports this).
    Denied,
    /// The thing named does not exist (a node, a quarantined item, a doctype).
    NotFound,
    /// The state does not allow it now (a node already revoked, a role change while the site
    /// runs, an item already closed).
    Conflict,
    /// A dependency (the hub, the enrolment store, the quarantine) is unreachable.
    Unavailable,
    /// The operation started and failed.
    Failed,
}

/// An executor's failure: a class and a reason code, nothing else.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("sync operation {kind:?}: {reason}")]
pub struct ExecError {
    /// The class of failure.
    pub kind: ExecKind,
    /// A short code of `[a-z0-9_]`, at most 64 characters.
    pub reason: String,
}

impl ExecError {
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
    /// The operation id is not one of the sync operations.
    #[error("`{op}` is not a sync operation")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_reasons_are_reduced_to_codes() {
        let e = ExecError::new(ExecKind::Failed, "Node branch07@example.com missing");
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
