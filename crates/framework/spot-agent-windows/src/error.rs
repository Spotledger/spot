//! The one error type of the Windows modules, with stable `agent_windows.*` codes.
//!
//! Callers match on [`WindowsError::code`], never on the message text; the codes are
//! catalogued in `spot-errors/catalogue/agent_windows.toml`. Messages name the operation,
//! a resource name or a parameter — never a password, a key or any other secret.

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-agent-windows";

/// The underlying Win32 or COM call failed.
pub const API: &str = "agent_windows.api";
/// An operation argument does not fit the module's own rules (a bad name, an empty list).
pub const INVALID_ARGUMENT: &str = "agent_windows.invalid_argument";
/// The named service does not exist.
pub const UNKNOWN_SERVICE: &str = "agent_windows.unknown_service";
/// Stopping or restarting a protected service needs the critical flag.
pub const SERVICE_PROTECTED: &str = "agent_windows.service_protected";
/// A service did not reach the expected state within the timeout.
pub const SERVICE_TIMEOUT: &str = "agent_windows.service_timeout";
/// The named update is not offered by the Windows Update Agent.
pub const UNKNOWN_UPDATE: &str = "agent_windows.unknown_update";
/// The update selection names neither ids nor a severity, or names both.
pub const UPDATE_SELECTION: &str = "agent_windows.update_selection";
/// The requested reboot instant is not a future whole second.
pub const REBOOT_TIME_INVALID: &str = "agent_windows.reboot_time_invalid";
/// The named firewall rule does not exist.
pub const UNKNOWN_RULE: &str = "agent_windows.unknown_rule";
/// Removing the agent's own outbound rule or the RDP rule needs the critical flag.
pub const RULE_PROTECTED: &str = "agent_windows.rule_protected";
/// The named firewall profile does not exist.
pub const UNKNOWN_PROFILE: &str = "agent_windows.unknown_profile";
/// The named local user does not exist.
pub const UNKNOWN_USER: &str = "agent_windows.unknown_user";
/// The named local group does not exist.
pub const UNKNOWN_GROUP: &str = "agent_windows.unknown_group";
/// Touching a built-in or otherwise protected account needs the explicit allow-list flag.
pub const USER_PROTECTED: &str = "agent_windows.user_protected";
/// The new secret could not be delivered on the sealed result channel.
pub const SECRET_DELIVERY: &str = "agent_windows.secret_delivery";
/// The Windows modules do not support this feature on this host.
pub const UNSUPPORTED: &str = "agent_windows.unsupported";
/// Verification after applying an operation did not confirm the new state.
pub const VERIFY_FAILED: &str = "agent_windows.verify_failed";

/// Why a Windows operation was refused or failed. Each variant carries a stable code
/// ([`WindowsError::code`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WindowsError {
    /// A Win32 or COM call failed; `detail` is a short, secret-free description.
    #[error("the Windows API call failed: {detail}")]
    Api {
        /// What the API reported, in words (never a secret).
        detail: String,
    },
    /// An operation argument is invalid beyond what the catalogue schema checks.
    #[error("the argument {param} is invalid: {detail}")]
    InvalidArgument {
        /// The parameter name.
        param: String,
        /// What is wrong, in words.
        detail: String,
    },
    /// No service with this name exists.
    #[error("no service named {name} exists")]
    UnknownService {
        /// The service name.
        name: String,
    },
    /// The service carries the agent's own management connection and the critical flag
    /// was not set.
    #[error("the service {name} is protected; the critical flag is required")]
    ServiceProtected {
        /// The service name.
        name: String,
    },
    /// The service did not reach the expected state before the timeout.
    #[error("the service {name} did not reach the expected state within {timeout_seconds} seconds")]
    ServiceTimeout {
        /// The service name.
        name: String,
        /// How long the module waited.
        timeout_seconds: u64,
    },
    /// No offered update has this id.
    #[error("no update with id {id} is offered by the Windows Update Agent")]
    UnknownUpdate {
        /// The update id.
        id: String,
    },
    /// The update selection is ambiguous or empty.
    #[error("the update selection is invalid: {detail}")]
    UpdateSelection {
        /// What is wrong, in words.
        detail: String,
    },
    /// The requested reboot instant is not a valid future whole second.
    #[error("the reboot instant {at} is not a valid future whole second")]
    RebootTimeInvalid {
        /// The rejected instant.
        at: i64,
    },
    /// No firewall rule with this name exists.
    #[error("no firewall rule named {name} exists")]
    UnknownRule {
        /// The rule name.
        name: String,
    },
    /// The rule carries the agent's own outbound connection or RDP and the critical flag
    /// was not set.
    #[error("the firewall rule {name} is protected; the critical flag is required")]
    RuleProtected {
        /// The rule name.
        name: String,
    },
    /// No firewall profile has this name.
    #[error("no firewall profile named {name} exists")]
    UnknownProfile {
        /// The profile name as the caller spelled it.
        name: String,
    },
    /// No local user with this name exists.
    #[error("no local user named {name} exists")]
    UnknownUser {
        /// The user name.
        name: String,
    },
    /// No local group with this name exists.
    #[error("no local group named {name} exists")]
    UnknownGroup {
        /// The group name.
        name: String,
    },
    /// The account is a built-in or configured protected account and the allow-list flag
    /// was not set.
    #[error("the account {name} is protected; the explicit allow-list flag is required")]
    UserProtected {
        /// The account name.
        name: String,
    },
    /// The generated secret could not be delivered on the sealed result channel.
    #[error("the new secret could not be delivered on the sealed result channel")]
    SecretDelivery,
    /// The host cannot provide this feature (a non-Windows build, a missing COM service).
    #[error("the Windows modules do not support {feature} on this host")]
    Unsupported {
        /// The feature that is unavailable.
        feature: String,
    },
    /// Applying the operation succeeded but verification did not confirm the new state.
    #[error("{op}: verification after apply failed: {detail}")]
    VerifyFailed {
        /// The operation id.
        op: String,
        /// What verification saw instead, in words.
        detail: String,
    },
}

impl WindowsError {
    /// The stable machine-readable code of this error.
    ///
    /// The codes, with their meanings:
    ///
    /// - `agent_windows.api` — a Win32 or COM call failed;
    /// - `agent_windows.invalid_argument` — an argument failed the module's own rules;
    /// - `agent_windows.unknown_service` — no such service;
    /// - `agent_windows.service_protected` — a protected service needs the critical flag;
    /// - `agent_windows.service_timeout` — a service missed its state deadline;
    /// - `agent_windows.unknown_update` — no such offered update;
    /// - `agent_windows.update_selection` — the update selection is ambiguous or empty;
    /// - `agent_windows.reboot_time_invalid` — the reboot instant is not a future second;
    /// - `agent_windows.unknown_rule` — no such firewall rule;
    /// - `agent_windows.rule_protected` — a protected rule needs the critical flag;
    /// - `agent_windows.unknown_profile` — no such firewall profile;
    /// - `agent_windows.unknown_user` — no such local user;
    /// - `agent_windows.unknown_group` — no such local group;
    /// - `agent_windows.user_protected` — a protected account needs the allow-list flag;
    /// - `agent_windows.secret_delivery` — the sealed result channel refused the secret;
    /// - `agent_windows.unsupported` — the host cannot provide the feature;
    /// - `agent_windows.verify_failed` — verification did not confirm the new state.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Api { .. } => API,
            Self::InvalidArgument { .. } => INVALID_ARGUMENT,
            Self::UnknownService { .. } => UNKNOWN_SERVICE,
            Self::ServiceProtected { .. } => SERVICE_PROTECTED,
            Self::ServiceTimeout { .. } => SERVICE_TIMEOUT,
            Self::UnknownUpdate { .. } => UNKNOWN_UPDATE,
            Self::UpdateSelection { .. } => UPDATE_SELECTION,
            Self::RebootTimeInvalid { .. } => REBOOT_TIME_INVALID,
            Self::UnknownRule { .. } => UNKNOWN_RULE,
            Self::RuleProtected { .. } => RULE_PROTECTED,
            Self::UnknownProfile { .. } => UNKNOWN_PROFILE,
            Self::UnknownUser { .. } => UNKNOWN_USER,
            Self::UnknownGroup { .. } => UNKNOWN_GROUP,
            Self::UserProtected { .. } => USER_PROTECTED,
            Self::SecretDelivery => SECRET_DELIVERY,
            Self::Unsupported { .. } => UNSUPPORTED,
            Self::VerifyFailed { .. } => VERIFY_FAILED,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn codes_are_dotted_lower_case() {
        for code in [
            API,
            INVALID_ARGUMENT,
            UNKNOWN_SERVICE,
            SERVICE_PROTECTED,
            SERVICE_TIMEOUT,
            UNKNOWN_UPDATE,
            UPDATE_SELECTION,
            REBOOT_TIME_INVALID,
            UNKNOWN_RULE,
            RULE_PROTECTED,
            UNKNOWN_PROFILE,
            UNKNOWN_USER,
            UNKNOWN_GROUP,
            USER_PROTECTED,
            SECRET_DELIVERY,
            UNSUPPORTED,
            VERIFY_FAILED,
        ] {
            assert!(code.starts_with("agent_windows."), "{code}");
            assert!(code.split('.').all(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase())
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            }));
        }
    }

    #[test]
    fn every_variant_has_its_own_code() {
        let errors = [
            WindowsError::Api {
                detail: "x".to_owned(),
            },
            WindowsError::InvalidArgument {
                param: "name".to_owned(),
                detail: "x".to_owned(),
            },
            WindowsError::UnknownService {
                name: "x".to_owned(),
            },
            WindowsError::ServiceProtected {
                name: "x".to_owned(),
            },
            WindowsError::ServiceTimeout {
                name: "x".to_owned(),
                timeout_seconds: 1,
            },
            WindowsError::UnknownUpdate { id: "x".to_owned() },
            WindowsError::UpdateSelection {
                detail: "x".to_owned(),
            },
            WindowsError::RebootTimeInvalid { at: 1 },
            WindowsError::UnknownRule {
                name: "x".to_owned(),
            },
            WindowsError::RuleProtected {
                name: "x".to_owned(),
            },
            WindowsError::UnknownProfile {
                name: "x".to_owned(),
            },
            WindowsError::UnknownUser {
                name: "x".to_owned(),
            },
            WindowsError::UnknownGroup {
                name: "x".to_owned(),
            },
            WindowsError::UserProtected {
                name: "x".to_owned(),
            },
            WindowsError::SecretDelivery,
            WindowsError::Unsupported {
                feature: "x".to_owned(),
            },
            WindowsError::VerifyFailed {
                op: "x".to_owned(),
                detail: "x".to_owned(),
            },
        ];
        let mut codes: Vec<&str> = errors.iter().map(WindowsError::code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), errors.len(), "each variant has its own code");
    }
}
