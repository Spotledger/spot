//! The `agent_linux_sys.*` failures: one stable code per reason a module refuses or a
//! machine operation fails.
//!
//! A [`SysError`] is a code and a plain-language message. Callers match on [`SysError::code`],
//! never on the text; `agent_linux_sys.toml` in `spot-errors` carries the catalogue entry for
//! each one, with a remedy an administrator can act on. Messages name paths, keys and
//! commands but never a secret.

/// `agent_linux_sys.command_failed`: a command exited with a non-zero status.
pub const COMMAND_FAILED: &str = "agent_linux_sys.command_failed";
/// `agent_linux_sys.command_timeout`: a command did not finish inside its timeout.
pub const COMMAND_TIMEOUT: &str = "agent_linux_sys.command_timeout";
/// `agent_linux_sys.command_output_too_large`: a command produced more output than the cap allows.
pub const COMMAND_OUTPUT_TOO_LARGE: &str = "agent_linux_sys.command_output_too_large";
/// `agent_linux_sys.tool_missing`: a command could not be started because the program is not installed.
pub const TOOL_MISSING: &str = "agent_linux_sys.tool_missing";
/// `agent_linux_sys.io`: a filesystem operation failed.
pub const IO: &str = "agent_linux_sys.io";
/// `agent_linux_sys.parse`: a tool produced output the module could not parse.
pub const PARSE: &str = "agent_linux_sys.parse";
/// `agent_linux_sys.invalid_argument`: a module argument failed its own validation.
pub const INVALID_ARGUMENT: &str = "agent_linux_sys.invalid_argument";
/// `agent_linux_sys.unsupported`: the machine does not offer the backend the operation needs.
pub const UNSUPPORTED: &str = "agent_linux_sys.unsupported";
/// `agent_linux_sys.refused`: a safety rule refused the change (see the message).
pub const REFUSED: &str = "agent_linux_sys.refused";
/// `agent_linux_sys.snapshot_missing`: a named snapshot does not exist.
pub const SNAPSHOT_MISSING: &str = "agent_linux_sys.snapshot_missing";
/// `agent_linux_sys.snapshot_target`: the provider contract target cannot carry a filesystem snapshot.
pub const SNAPSHOT_TARGET: &str = "agent_linux_sys.snapshot_target";
/// `agent_linux_sys.network_revert_required`: a connectivity-affecting change awaits confirmation.
pub const NETWORK_REVERT_REQUIRED: &str = "agent_linux_sys.network_revert_required";
/// `agent_linux_sys.network_revert_expired`: the confirmation window passed and the change was reverted.
pub const NETWORK_REVERT_EXPIRED: &str = "agent_linux_sys.network_revert_expired";
/// `agent_linux_sys.network_revert_corrupt`: a pending revert ticket and its backup are unreadable.
pub const NETWORK_REVERT_CORRUPT: &str = "agent_linux_sys.network_revert_corrupt";
/// `agent_linux_sys.network_revert_schedule`: the independent revert timer could not be scheduled.
pub const NETWORK_REVERT_SCHEDULE: &str = "agent_linux_sys.network_revert_schedule";
/// `agent_linux_sys.network_revert_busy`: another process holds the revert lock; retry shortly.
pub const NETWORK_REVERT_BUSY: &str = "agent_linux_sys.network_revert_busy";
/// `agent_linux_sys.network_backup_incomplete`: the previous gateway or DNS could not be read, so the change was not started.
pub const NETWORK_BACKUP_INCOMPLETE: &str = "agent_linux_sys.network_backup_incomplete";
/// `agent_linux_sys.network_no_backend`: neither NetworkManager nor systemd-networkd is active.
pub const NETWORK_NO_BACKEND: &str = "agent_linux_sys.network_no_backend";
/// `agent_linux_sys.cert_missing`: a named certificate is not in the trust store.
pub const CERT_MISSING: &str = "agent_linux_sys.cert_missing";
/// `agent_linux_sys.cert_invalid`: a certificate could not be parsed.
pub const CERT_INVALID: &str = "agent_linux_sys.cert_invalid";
/// `agent_linux_sys.cert_chain`: a certificate does not verify against the trust store.
pub const CERT_CHAIN: &str = "agent_linux_sys.cert_chain";
/// `agent_linux_sys.time_jump_too_large`: a forced step would move the clock beyond the guard.
pub const TIME_JUMP_TOO_LARGE: &str = "agent_linux_sys.time_jump_too_large";
/// `agent_linux_sys.sysctl_key`: a kernel parameter is not on the Spotledger allow-list.
pub const SYSCTL_KEY: &str = "agent_linux_sys.sysctl_key";
/// `agent_linux_sys.sysctl_value`: a kernel parameter value is outside its accepted range.
pub const SYSCTL_VALUE: &str = "agent_linux_sys.sysctl_value";
/// `agent_linux_sys.mac_disable_refused`: a mode change would weaken mandatory access control.
pub const MAC_DISABLE_REFUSED: &str = "agent_linux_sys.mac_disable_refused";
/// `agent_linux_sys.mac_unknown`: the mandatory access control system is not recognised.
pub const MAC_UNKNOWN: &str = "agent_linux_sys.mac_unknown";

/// A Linux system module failure: a stable `agent_linux_sys.*` code and a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct SysError {
    /// The stable machine-readable code, for the API error envelope.
    pub code: &'static str,
    /// What went wrong, phrased for the administrator.
    pub message: String,
}

impl SysError {
    /// Builds a failure from its code and message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A filesystem failure ([`IO`]).
    #[must_use]
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(IO, message)
    }

    /// A tool-output parse failure ([`PARSE`]).
    #[must_use]
    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(PARSE, message)
    }

    /// A refusal by one of the modules' safety rules ([`REFUSED`]).
    #[must_use]
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(REFUSED, message)
    }

    /// An argument that failed validation ([`INVALID_ARGUMENT`]).
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(INVALID_ARGUMENT, message)
    }

    /// A backend the machine does not offer ([`UNSUPPORTED`]).
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(UNSUPPORTED, message)
    }
}

impl From<std::io::Error> for SysError {
    fn from(source: std::io::Error) -> Self {
        Self::io(source.to_string())
    }
}

/// The crate's result alias.
pub type SysResult<T> = Result<T, SysError>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn codes_are_dotted_lower_case_and_unique() {
        let mut codes = vec![
            COMMAND_FAILED,
            COMMAND_TIMEOUT,
            COMMAND_OUTPUT_TOO_LARGE,
            TOOL_MISSING,
            IO,
            PARSE,
            INVALID_ARGUMENT,
            UNSUPPORTED,
            REFUSED,
            SNAPSHOT_MISSING,
            SNAPSHOT_TARGET,
            NETWORK_REVERT_REQUIRED,
            NETWORK_REVERT_EXPIRED,
            NETWORK_REVERT_CORRUPT,
            NETWORK_REVERT_SCHEDULE,
            NETWORK_REVERT_BUSY,
            NETWORK_BACKUP_INCOMPLETE,
            NETWORK_NO_BACKEND,
            CERT_MISSING,
            CERT_INVALID,
            CERT_CHAIN,
            TIME_JUMP_TOO_LARGE,
            SYSCTL_KEY,
            SYSCTL_VALUE,
            MAC_DISABLE_REFUSED,
            MAC_UNKNOWN,
        ];
        let count = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), count, "codes must be unique");
        for code in codes {
            assert!(code.starts_with("agent_linux_sys."), "{code}");
            assert!(code.split('.').all(|segment| !segment.is_empty()), "{code}");
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'),
                "{code}"
            );
        }
    }

    #[test]
    fn the_error_renders_its_code_and_message() {
        let error = SysError::refused("no revert path");
        assert_eq!(error.code, REFUSED);
        assert_eq!(error.to_string(), "agent_linux_sys.refused: no revert path");
    }
}
