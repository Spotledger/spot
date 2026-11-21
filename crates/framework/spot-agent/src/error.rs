//! The agent's error type with stable `agent.*` codes. No variant carries a key, a token,
//! a payload or personal data: texts name the failing step only.

use std::fmt;

/// Every failure of the agent executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// `agent.toml` is malformed, has an unknown key or an invalid value.
    ConfigInvalid(String),
    /// The configuration or enrolment record is missing: the agent is not enrolled.
    NotEnrolled,
    /// The agent is already enrolled (enrol refuses to overwrite an identity).
    AlreadyEnrolled,
    /// The key store kind is not available on this host (TPM stub).
    KeyStoreUnavailable,
    /// A state file or directory is readable or writable beyond its owner.
    StatePermissions(String),
    /// A state file could not be read or written.
    StateIo(String),
    /// The gateway refused the enrolment token (one answer for every cause).
    TokenInvalid,
    /// The gateway answer was malformed or violated the contract.
    GatewayProtocol(String),
    /// The gateway could not be reached or the transport failed.
    Transport(String),
    /// No transport is wired in this build (the channel client is WP-ADM-02.T3).
    TransportUnavailable,
    /// The local audit log failed (fail closed).
    AuditFailed(String),
    /// A command-line argument is wrong.
    Usage(String),
    /// The host has no hardware-backed key store and the operator did not accept software
    /// keys (`--allow-software-keys`), or a rotation would loosen the stored policy.
    SoftwareKeysNotAllowed,
}

impl AgentError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid(_) => "agent.config_invalid",
            Self::NotEnrolled => "agent.not_enrolled",
            Self::AlreadyEnrolled => "agent.already_enrolled",
            Self::KeyStoreUnavailable => "agent.key_store_unavailable",
            Self::StatePermissions(_) => "agent.state_permissions",
            Self::StateIo(_) => "agent.state_io",
            Self::TokenInvalid => "agent.token_invalid",
            Self::GatewayProtocol(_) => "agent.gateway_protocol",
            Self::Transport(_) => "agent.transport",
            Self::TransportUnavailable => "agent.transport_unavailable",
            Self::AuditFailed(_) => "agent.audit_failed",
            Self::Usage(_) => "agent.usage",
            Self::SoftwareKeysNotAllowed => "agent.software_keys_not_allowed",
        }
    }

    /// The process exit code: 2 usage, 3 not enrolled / already enrolled, 4 permissions,
    /// 1 everything else.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) | Self::SoftwareKeysNotAllowed => 2,
            Self::NotEnrolled | Self::AlreadyEnrolled => 3,
            Self::StatePermissions(_) => 4,
            _ => 1,
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::ConfigInvalid(d)
            | Self::StatePermissions(d)
            | Self::StateIo(d)
            | Self::GatewayProtocol(d)
            | Self::Transport(d)
            | Self::AuditFailed(d)
            | Self::Usage(d) => d.as_str(),
            Self::NotEnrolled => "the agent is not enrolled",
            Self::AlreadyEnrolled => "the agent is already enrolled",
            Self::KeyStoreUnavailable => "the requested key store is not available on this host",
            Self::TokenInvalid => "the enrolment token was refused",
            Self::TransportUnavailable => "no gateway transport is available in this build",
            Self::SoftwareKeysNotAllowed => {
                "this host has no hardware-backed key store; pass --allow-software-keys at \
                 enrolment to accept software keys (the choice is recorded in the audit log)"
            }
        };
        write!(f, "{}: {}", self.code(), detail)
    }
}

impl std::error::Error for AgentError {}
