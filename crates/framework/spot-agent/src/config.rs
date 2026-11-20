//! `agent.toml`: strictly validated; an unknown key refuses start.

use serde::{Deserialize, Serialize};

use crate::error::AgentError;

/// Log verbosity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warn,
    /// The default.
    Info,
    /// Verbose, still redacted.
    Debug,
}

impl LogLevel {
    /// The lower-case name used in log lines.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

fn default_poll_seconds() -> u64 {
    5
}

fn default_log_level() -> LogLevel {
    LogLevel::Info
}

/// The default pause between self-update checks, in hours.
pub const DEFAULT_UPDATE_CHECK_HOURS: u64 = 6;

fn default_update_check_hours() -> u64 {
    DEFAULT_UPDATE_CHECK_HOURS
}

/// The default install root (`versions/`, `current`, `update-trust.json`).
#[must_use]
pub fn default_install_root() -> &'static str {
    if cfg!(windows) {
        "C:\\Program Files\\Spotledger\\Agent"
    } else {
        "/opt/spot-agent"
    }
}

/// The agent configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// The gateway base URL (`https://host[:port]`).
    pub gateway: String,
    /// Path of the PEM file holding the gateway trust anchor certificate, when private.
    #[serde(default)]
    pub ca: Option<String>,
    /// Pause between polls in seconds, 1 to 3600.
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// Log verbosity.
    #[serde(default = "default_log_level")]
    pub log_level: LogLevel,
    /// The operation namespaces this agent may execute; empty by default.
    #[serde(default)]
    pub allowed_modules: Vec<String>,
    /// Hours between self-update checks (1 to 168); `0` turns the scheduled check off. The
    /// signature checks of an update are never optional. Each pause gets a random extra of
    /// up to 20 per cent so a fleet does not call the gateway at the same minute.
    #[serde(default = "default_update_check_hours")]
    pub update_check_hours: u64,
    /// The install root the updater manages; [`default_install_root`] when absent.
    #[serde(default)]
    pub install_root: Option<String>,
}

impl AgentConfig {
    /// A configuration for `gateway` with the defaults.
    #[must_use]
    pub fn new(gateway: impl Into<String>, ca: Option<String>) -> Self {
        Self {
            gateway: gateway.into(),
            ca,
            poll_seconds: default_poll_seconds(),
            log_level: default_log_level(),
            allowed_modules: Vec::new(),
            update_check_hours: default_update_check_hours(),
            install_root: None,
        }
    }

    /// The install root in effect.
    #[must_use]
    pub fn install_root(&self) -> &str {
        self.install_root
            .as_deref()
            .unwrap_or(default_install_root())
    }

    /// Parses and validates the TOML text.
    ///
    /// # Errors
    /// [`AgentError::ConfigInvalid`] on a parse error (including unknown keys) or a bad value.
    pub fn parse(text: &str) -> Result<Self, AgentError> {
        let cfg: Self = toml::from_str(text)
            .map_err(|e| AgentError::ConfigInvalid(format!("agent.toml: {}", e.message())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Serialises to TOML.
    ///
    /// # Errors
    /// [`AgentError::ConfigInvalid`] if serialisation fails.
    pub fn to_toml(&self) -> Result<String, AgentError> {
        toml::to_string(self).map_err(|_| AgentError::ConfigInvalid("cannot serialise".to_owned()))
    }

    /// Field checks.
    ///
    /// # Errors
    /// [`AgentError::ConfigInvalid`] naming the field.
    pub fn validate(&self) -> Result<(), AgentError> {
        let bad = |f: &str| AgentError::ConfigInvalid(format!("invalid value for `{f}`"));
        let Some(rest) = self.gateway.strip_prefix("https://") else {
            return Err(bad("gateway"));
        };
        if rest.is_empty()
            || rest.len() > 253
            || rest.contains(|c: char| c.is_whitespace() || c == '@' || c == '?' || c == '#')
        {
            return Err(bad("gateway"));
        }
        if !(1..=3600).contains(&self.poll_seconds) {
            return Err(bad("poll_seconds"));
        }
        if self.update_check_hours > 168 {
            return Err(bad("update_check_hours"));
        }
        if let Some(root) = &self.install_root
            && (root.is_empty() || root.len() > 260 || root.contains('\0'))
        {
            return Err(bad("install_root"));
        }
        for module in &self.allowed_modules {
            if !is_namespace(module) {
                return Err(bad("allowed_modules"));
            }
        }
        Ok(())
    }

    /// Whether the operation id (`server.service.restart`) falls in an allowed namespace.
    #[must_use]
    pub fn allows(&self, op: &str) -> bool {
        self.allowed_modules.iter().any(|m| {
            op == m
                || op
                    .strip_prefix(m.as_str())
                    .is_some_and(|r| r.starts_with('.'))
        })
    }
}

/// A namespace: dot-separated segments of `[a-z][a-z0-9_-]*`, at most 120 bytes.
#[must_use]
pub fn is_namespace(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 120
        && text.split('.').all(|seg| {
            let mut chars = seg.chars();
            chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        })
}
