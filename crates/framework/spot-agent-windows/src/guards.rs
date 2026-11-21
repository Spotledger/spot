//! The safety guards: what the agent must never break by accident.
//!
//! A management agent can lock itself out. The guards are the named resources that carry
//! the agent's own connection or that a customer must keep working:
//!
//! - the **agent service** and the **Spotledger runtime service**, which stopping would
//!   cut the management session;
//! - the **firewall rule that carries the agent's outbound connection** and the **RDP
//!   rules**, whose removal would cut remote access;
//! - the **built-in accounts** (`Administrator`, `Guest`, `DefaultAccount`,
//!   `WDAGUtilityAccount`) and the **Administrators** group, which an operation may touch
//!   only with an explicit allow-list flag.
//!
//! Every guard is a pure function of the [`AgentConfig`] and the operation arguments; a
//! refusal is a typed [`WindowsError`] raised before any change.

use crate::error::WindowsError;

/// The names the guards treat as the agent's own or as built-in.
///
/// The defaults cover a stock Windows install; a host overrides them to match its own
/// service names, firewall rules and accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConfig {
    /// The agent's own Windows service name.
    pub agent_service: String,
    /// The Spotledger runtime's Windows service name.
    pub runtime_service: String,
    /// Further service names that must not be stopped without the critical flag.
    pub extra_protected_services: Vec<String>,
    /// The firewall rules that carry the agent's outbound management connection.
    pub agent_outbound_rules: Vec<String>,
    /// The firewall rules that carry Remote Desktop.
    pub rdp_rules: Vec<String>,
    /// The accounts that may be changed only with the allow-list flag.
    pub protected_accounts: Vec<String>,
    /// The local groups that may be changed only with the allow-list flag.
    pub protected_groups: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_service: "spot-agent".to_owned(),
            runtime_service: "spot-runtime".to_owned(),
            extra_protected_services: Vec::new(),
            agent_outbound_rules: vec!["Spotledger Agent (outbound)".to_owned()],
            rdp_rules: vec![
                "Remote Desktop - User Mode (TCP-In)".to_owned(),
                "Remote Desktop - User Mode (UDP-In)".to_owned(),
            ],
            protected_accounts: vec![
                "Administrator".to_owned(),
                "Guest".to_owned(),
                "DefaultAccount".to_owned(),
                "WDAGUtilityAccount".to_owned(),
            ],
            protected_groups: vec!["Administrators".to_owned()],
        }
    }
}

/// Whether two names are the same, ignoring ASCII case (Windows names are
/// case-insensitive).
fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl AgentConfig {
    /// Whether `name` is the agent service, the runtime service or an extra protected one.
    #[must_use]
    pub fn is_protected_service(&self, name: &str) -> bool {
        same(name, &self.agent_service)
            || same(name, &self.runtime_service)
            || self
                .extra_protected_services
                .iter()
                .any(|protected| same(protected, name))
    }

    /// Whether `name` is a rule that carries the agent's outbound connection or RDP.
    #[must_use]
    pub fn is_protected_rule(&self, name: &str) -> bool {
        self.agent_outbound_rules
            .iter()
            .chain(self.rdp_rules.iter())
            .any(|protected| same(protected, name))
    }

    /// Whether `name` is a built-in or otherwise protected account.
    #[must_use]
    pub fn is_protected_account(&self, name: &str) -> bool {
        self.protected_accounts
            .iter()
            .any(|protected| same(protected, name))
    }

    /// Whether `name` is a protected group.
    #[must_use]
    pub fn is_protected_group(&self, name: &str) -> bool {
        self.protected_groups
            .iter()
            .any(|protected| same(protected, name))
    }

    /// Refuses to stop or restart a protected service without the critical flag.
    ///
    /// # Errors
    /// [`WindowsError::ServiceProtected`] when the service is protected and the flag is
    /// not set.
    pub fn guard_service(&self, name: &str, allow_critical: bool) -> Result<(), WindowsError> {
        if !allow_critical && self.is_protected_service(name) {
            return Err(WindowsError::ServiceProtected {
                name: name.to_owned(),
            });
        }
        Ok(())
    }

    /// Refuses to add or remove a protected firewall rule without the critical flag.
    ///
    /// # Errors
    /// [`WindowsError::RuleProtected`] when the rule is protected and the flag is not set.
    pub fn guard_rule(&self, name: &str, allow_critical: bool) -> Result<(), WindowsError> {
        if !allow_critical && self.is_protected_rule(name) {
            return Err(WindowsError::RuleProtected {
                name: name.to_owned(),
            });
        }
        Ok(())
    }

    /// Refuses to change a protected account without the allow-list flag.
    ///
    /// # Errors
    /// [`WindowsError::UserProtected`] when the account is protected and the flag is not
    /// set.
    pub fn guard_account(&self, name: &str, allow_protected: bool) -> Result<(), WindowsError> {
        if !allow_protected && self.is_protected_account(name) {
            return Err(WindowsError::UserProtected {
                name: name.to_owned(),
            });
        }
        Ok(())
    }

    /// Refuses to change a protected group without the allow-list flag.
    ///
    /// # Errors
    /// [`WindowsError::UserProtected`] when the group is protected and the flag is not
    /// set.
    pub fn guard_group(&self, name: &str, allow_protected: bool) -> Result<(), WindowsError> {
        if !allow_protected && self.is_protected_group(name) {
            return Err(WindowsError::UserProtected {
                name: name.to_owned(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_default_config_protects_the_agent_and_the_built_ins() {
        let config = AgentConfig::default();
        assert!(config.is_protected_service("spot-agent"));
        assert!(config.is_protected_service("SPOT-RUNTIME"));
        assert!(!config.is_protected_service("wuauserv"));
        assert!(config.is_protected_account("administrator"));
        assert!(!config.is_protected_account("ana"));
        assert!(config.is_protected_group("Administrators"));
    }

    #[test]
    fn the_service_guard_requires_the_critical_flag() {
        let config = AgentConfig::default();
        assert_eq!(
            config
                .guard_service("spot-runtime", false)
                .unwrap_err()
                .code(),
            crate::error::SERVICE_PROTECTED
        );
        assert!(config.guard_service("spot-runtime", true).is_ok());
        assert!(config.guard_service("wuauserv", false).is_ok());
    }

    #[test]
    fn the_rule_guard_covers_rdp_and_the_agent_rule() {
        let config = AgentConfig::default();
        assert!(
            config
                .guard_rule("Remote Desktop - User Mode (TCP-In)", false)
                .is_err()
        );
        assert!(
            config
                .guard_rule("Spotledger Agent (outbound)", false)
                .is_err()
        );
        assert!(config.guard_rule("Web (HTTP-In)", false).is_ok());
        assert!(config.guard_rule("Web (HTTP-In)", true).is_ok());
    }
}
