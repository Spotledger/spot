//! The Linux module error vocabulary, with stable `agent_linux.*` codes.
//!
//! Every failure a Linux operation module can report is a [`LinuxError`] carrying a stable,
//! machine-readable code ([`LinuxError::code`]). The codes are catalogued in
//! `spot-errors/catalogue/agent_linux.toml`; they are a library-internal vocabulary (the host
//! agent bridges them onto the wire result), so every catalogue entry is class `none`.
//!
//! Messages name the operation, the program or the path at fault and never a secret, a
//! parameter value or the raw bytes of a command's output.

use thiserror::Error;

/// The distribution (or its package backend) is not one this module knows how to drive.
pub const UNSUPPORTED: &str = "agent_linux.unsupported";
/// A required tool, file or backend is missing on this host.
pub const UNAVAILABLE: &str = "agent_linux.unavailable";
/// An argument failed the module's own validation, after the catalogue's schema check.
pub const INVALID_ARGUMENT: &str = "agent_linux.invalid_argument";
/// A command could not be spawned at all.
pub const COMMAND_SPAWN: &str = "agent_linux.command_spawn";
/// A command ran and exited with a non-zero status.
pub const COMMAND_FAILED: &str = "agent_linux.command_failed";
/// A command did not finish within its timeout and was killed.
pub const COMMAND_TIMEOUT: &str = "agent_linux.command_timeout";
/// A command produced more output than the cap allows.
pub const OUTPUT_TOO_LARGE: &str = "agent_linux.output_too_large";
/// The package manager is busy: another transaction holds its lock.
pub const PACKAGE_LOCKED: &str = "agent_linux.package_locked";
/// Refused to stop, restart, disable or mask a protected unit without the critical flag.
pub const PROTECTED_UNIT: &str = "agent_linux.protected_unit";
/// Refused to touch an account below the normal-user UID floor without the allow-list.
pub const PROTECTED_UID: &str = "agent_linux.protected_uid";
/// Refused to remove the firewall rule that carries management access without the critical flag.
pub const LOCKOUT_GUARD: &str = "agent_linux.lockout_guard";
/// A filesystem read failed.
pub const IO: &str = "agent_linux.io";
/// A tool's output could not be parsed.
pub const PARSE: &str = "agent_linux.parse";
/// Verification after an apply found the desired state was not reached.
pub const VERIFY_FAILED: &str = "agent_linux.verify_failed";

/// Why a Linux operation could not be planned, applied or verified.
///
/// Every variant carries a stable code ([`LinuxError::code`]) and a short, secret-free
/// description. `Debug` prints the same fields, so a dumped error never leaks a value.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum LinuxError {
    /// The distribution or backend is not supported (see [`UNSUPPORTED`]).
    #[error("{what} is not a supported Linux distribution or backend")]
    Unsupported {
        /// What was not recognised (a distribution id, a firewall backend).
        what: String,
    },
    /// A required tool, file or backend is missing (see [`UNAVAILABLE`]).
    #[error("required tool or file is unavailable: {what}")]
    Unavailable {
        /// What is missing.
        what: String,
    },
    /// An argument failed the module's validation (see [`INVALID_ARGUMENT`]).
    #[error("{op}: parameter {param} is invalid: {detail}")]
    InvalidArgument {
        /// The operation id.
        op: String,
        /// The parameter name.
        param: String,
        /// Why it was refused, in words (never the value).
        detail: String,
    },
    /// A command could not be spawned (see [`COMMAND_SPAWN`]).
    #[error("could not run {program}: {detail}")]
    CommandSpawn {
        /// The program that could not be spawned.
        program: String,
        /// The operating system's message.
        detail: String,
    },
    /// A command exited non-zero (see [`COMMAND_FAILED`]).
    #[error("{program} exited with status {status}: {detail}")]
    CommandFailed {
        /// The program that failed.
        program: String,
        /// Its exit status.
        status: i32,
        /// A short, secret-free excerpt of the failure.
        detail: String,
    },
    /// A command timed out (see [`COMMAND_TIMEOUT`]).
    #[error("{program} did not finish within {timeout_secs}s and was killed")]
    CommandTimeout {
        /// The program that timed out.
        program: String,
        /// The timeout that elapsed, in whole seconds.
        timeout_secs: u64,
    },
    /// A command's output exceeded the cap (see [`OUTPUT_TOO_LARGE`]).
    #[error("{program} produced more than {cap} bytes of output")]
    OutputTooLarge {
        /// The program whose output overflowed.
        program: String,
        /// The cap, in bytes.
        cap: usize,
    },
    /// The package manager is locked (see [`PACKAGE_LOCKED`]).
    #[error("the {backend} package manager is busy: another transaction holds its lock")]
    PackageLocked {
        /// The backend's short name (`dnf` or `apt`).
        backend: String,
    },
    /// A protected unit was targeted without the critical flag (see [`PROTECTED_UNIT`]).
    #[error("{unit} is a protected unit and may not be changed without the critical flag")]
    ProtectedUnit {
        /// The unit at fault.
        unit: String,
    },
    /// An account below the UID floor was targeted (see [`PROTECTED_UID`]).
    #[error("account {name} has UID {uid} below the normal-user floor and is not allow-listed")]
    ProtectedUid {
        /// The account name.
        name: String,
        /// Its numeric UID.
        uid: u32,
    },
    /// A firewall rule carrying management access was about to be removed (see
    /// [`LOCKOUT_GUARD`]).
    #[error("refusing to remove {rule}: it carries management access and needs the critical flag")]
    LockoutGuard {
        /// The rule at fault.
        rule: String,
    },
    /// A filesystem read failed (see [`IO`]).
    #[error("could not read {path}: {detail}")]
    Io {
        /// The path at fault.
        path: String,
        /// The operating system's message.
        detail: String,
    },
    /// A tool's output could not be parsed (see [`PARSE`]).
    #[error("could not parse {what}: {detail}")]
    Parse {
        /// What was being parsed.
        what: String,
        /// Why parsing failed, in words (never the raw output).
        detail: String,
    },
    /// Verification after apply failed (see [`VERIFY_FAILED`]).
    #[error("{op}: verification failed: {detail}")]
    VerifyFailed {
        /// The operation id.
        op: String,
        /// What the verification found.
        detail: String,
    },
}

impl LinuxError {
    /// The stable machine-readable code of this error. Never a parameter value.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported { .. } => UNSUPPORTED,
            Self::Unavailable { .. } => UNAVAILABLE,
            Self::InvalidArgument { .. } => INVALID_ARGUMENT,
            Self::CommandSpawn { .. } => COMMAND_SPAWN,
            Self::CommandFailed { .. } => COMMAND_FAILED,
            Self::CommandTimeout { .. } => COMMAND_TIMEOUT,
            Self::OutputTooLarge { .. } => OUTPUT_TOO_LARGE,
            Self::PackageLocked { .. } => PACKAGE_LOCKED,
            Self::ProtectedUnit { .. } => PROTECTED_UNIT,
            Self::ProtectedUid { .. } => PROTECTED_UID,
            Self::LockoutGuard { .. } => LOCKOUT_GUARD,
            Self::Io { .. } => IO,
            Self::Parse { .. } => PARSE,
            Self::VerifyFailed { .. } => VERIFY_FAILED,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn codes_are_dotted_lower_case_under_the_crate_prefix() {
        let errors = [
            LinuxError::Unsupported {
                what: "plan9".to_owned(),
            },
            LinuxError::Unavailable {
                what: "dnf".to_owned(),
            },
            LinuxError::InvalidArgument {
                op: "os.package.install".to_owned(),
                param: "packages".to_owned(),
                detail: "empty".to_owned(),
            },
            LinuxError::CommandSpawn {
                program: "dnf".to_owned(),
                detail: "no such file".to_owned(),
            },
            LinuxError::CommandFailed {
                program: "dnf".to_owned(),
                status: 1,
                detail: "failed".to_owned(),
            },
            LinuxError::CommandTimeout {
                program: "dnf".to_owned(),
                timeout_secs: 1,
            },
            LinuxError::OutputTooLarge {
                program: "dnf".to_owned(),
                cap: 1,
            },
            LinuxError::PackageLocked {
                backend: "dnf".to_owned(),
            },
            LinuxError::ProtectedUnit {
                unit: "spot-agent.service".to_owned(),
            },
            LinuxError::ProtectedUid {
                name: "root".to_owned(),
                uid: 0,
            },
            LinuxError::LockoutGuard {
                rule: "22/tcp".to_owned(),
            },
            LinuxError::Io {
                path: "/etc/passwd".to_owned(),
                detail: "missing".to_owned(),
            },
            LinuxError::Parse {
                what: "rpm output".to_owned(),
                detail: "no line".to_owned(),
            },
            LinuxError::VerifyFailed {
                op: "os.service.start".to_owned(),
                detail: "still inactive".to_owned(),
            },
        ];
        for error in errors {
            let code = error.code();
            assert!(code.starts_with("agent_linux."), "{code}");
            assert!(
                code.split('.').all(|s| {
                    !s.is_empty()
                        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                        && s.chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                }),
                "{code}"
            );
        }
    }

    #[test]
    fn messages_never_carry_the_raw_value() {
        let error = LinuxError::InvalidArgument {
            op: "os.package.install".to_owned(),
            param: "packages".to_owned(),
            detail: "contains a leading dash".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("os.package.install"));
        assert!(text.contains("leading dash"));
    }
}
