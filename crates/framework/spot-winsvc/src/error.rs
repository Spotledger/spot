//! The typed error every `spot-winsvc` operation returns, and its silent exit code.
//!
//! Exit codes (brief, section "Silent semantics"): `0` ok, `1` usage, `2` a missing
//! prerequisite (named), `3` failed. Never includes a secret, token or payload — every
//! variant carries only a path, a name or a short reason.

use std::fmt;

/// Everything that can go wrong running the service host.
#[derive(Debug)]
pub enum WinsvcError {
    /// Bad command-line usage (exit code 1).
    Usage(String),
    /// A named prerequisite is missing (exit code 2): a binary, a directory, a platform
    /// feature (the Service Control Manager on a non-Windows host).
    PrerequisiteMissing(String),
    /// The operation ran but failed (exit code 3).
    Failed(String),
    /// The PostgreSQL supervisor reported an error.
    Supervisor(spot_pg_supervisor::SupervisorError),
    /// The updater layer reported an error (upgrade pointer, archive, policy).
    Updater(spot_updater::UpdaterError),
    /// An I/O failure, reported without the path's contents, only its role.
    Io {
        /// What was being done, e.g. "reading service config".
        context: String,
        /// The underlying message (no secret ever flows through this path).
        message: String,
    },
}

impl WinsvcError {
    /// A usage error with the given message.
    pub fn usage(message: impl Into<String>) -> Self {
        WinsvcError::Usage(message.into())
    }

    /// A missing-prerequisite error naming what is missing.
    pub fn prerequisite(name: impl Into<String>) -> Self {
        WinsvcError::PrerequisiteMissing(name.into())
    }

    /// A plain operation failure.
    pub fn failed(message: impl Into<String>) -> Self {
        WinsvcError::Failed(message.into())
    }

    /// Wraps an I/O error with the context of what was being attempted.
    pub fn io(context: impl Into<String>, err: &std::io::Error) -> Self {
        WinsvcError::Io {
            context: context.into(),
            message: err.to_string(),
        }
    }

    /// The process exit code for this error (brief: 1 usage, 2 prerequisite, 3 failed).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            WinsvcError::Usage(_) => 1,
            WinsvcError::PrerequisiteMissing(_) => 2,
            WinsvcError::Failed(_) | WinsvcError::Io { .. } => 3,
            WinsvcError::Supervisor(_) | WinsvcError::Updater(_) => 3,
        }
    }
}

impl fmt::Display for WinsvcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WinsvcError::Usage(m) => write!(f, "usage: {m}"),
            WinsvcError::PrerequisiteMissing(name) => write!(f, "missing prerequisite: {name}"),
            WinsvcError::Failed(m) => write!(f, "failed: {m}"),
            WinsvcError::Supervisor(e) => write!(f, "postgresql supervisor: {e}"),
            WinsvcError::Updater(e) => write!(f, "updater: {e}"),
            WinsvcError::Io { context, message } => write!(f, "{context}: {message}"),
        }
    }
}

impl std::error::Error for WinsvcError {}

impl From<spot_pg_supervisor::SupervisorError> for WinsvcError {
    fn from(err: spot_pg_supervisor::SupervisorError) -> Self {
        WinsvcError::Supervisor(err)
    }
}

impl From<spot_updater::UpdaterError> for WinsvcError {
    fn from(err: spot_updater::UpdaterError) -> Self {
        WinsvcError::Updater(err)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn exit_codes_match_the_brief() {
        assert_eq!(WinsvcError::usage("x").exit_code(), 1);
        assert_eq!(WinsvcError::prerequisite("postgres").exit_code(), 2);
        assert_eq!(WinsvcError::failed("x").exit_code(), 3);
    }

    #[test]
    fn display_never_echoes_more_than_the_given_message() {
        let err = WinsvcError::prerequisite("the SCM");
        assert_eq!(err.to_string(), "missing prerequisite: the SCM");
    }
}
