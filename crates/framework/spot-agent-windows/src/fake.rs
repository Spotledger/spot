//! [`FakeWin`]: a complete, in-memory [`WinApi`] for tests.
//!
//! The fake is a small model of a Windows server — services with states and recovery
//! policies, offered updates, firewall rules and profiles, and local users and groups —
//! behind the same trait the real implementation satisfies. Every test in this crate and
//! in `tests/` runs against it on Linux, so the module logic is exercised without a
//! Windows host. It never stores a password value: a create or a reset records only the
//! secret's length, so a test can prove a password was set without holding it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::api::{
    Direction, FirewallRule, LocalGroup, LocalUser, Profile, Protocol, RecoveryAction,
    RecoveryConfig, RuleAction, ServiceInfo, ServiceState, Severity, StartType, UpdateInfo, WinApi,
};
use crate::error::WindowsError;
use crate::secrets::Secret;

/// The fake's mutable state.
#[derive(Debug)]
struct State {
    services: BTreeMap<String, ServiceInfo>,
    recovery: BTreeMap<String, RecoveryConfig>,
    updates: BTreeMap<String, UpdateInfo>,
    rules: BTreeMap<String, FirewallRule>,
    profiles: BTreeMap<Profile, bool>,
    users: BTreeMap<String, LocalUser>,
    groups: BTreeMap<String, LocalGroup>,
    /// The length of each password a create or reset installed, never the value.
    password_lengths: BTreeMap<String, usize>,
    reboot_required: bool,
    scheduled_reboot: Option<i64>,
}

/// An in-memory [`WinApi`].
pub struct FakeWin {
    state: Mutex<State>,
}

impl FakeWin {
    /// An empty machine: no services, no updates, no rules, no users.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                services: BTreeMap::new(),
                recovery: BTreeMap::new(),
                updates: BTreeMap::new(),
                rules: BTreeMap::new(),
                profiles: BTreeMap::new(),
                users: BTreeMap::new(),
                groups: BTreeMap::new(),
                password_lengths: BTreeMap::new(),
                reboot_required: false,
                scheduled_reboot: None,
            }),
        }
    }

    /// A machine with a representative stock of services, updates, firewall rules and
    /// users, which every module test starts from.
    #[must_use]
    pub fn stock() -> Self {
        let fake = Self::with_stock_services();
        fake.with_updates_defaults();
        fake.with_firewall_defaults();
        fake.with_users_defaults();
        fake
    }

    /// A machine with only the stock services.
    #[must_use]
    pub fn with_stock_services() -> Self {
        let fake = Self::new();
        {
            let mut state = fake.lock();
            for (name, display, service_state, start_type) in [
                (
                    "bits",
                    "Background Intelligent Transfer Service",
                    ServiceState::Stopped,
                    StartType::Manual,
                ),
                (
                    "spooler",
                    "Print Spooler",
                    ServiceState::Stopped,
                    StartType::Automatic,
                ),
                (
                    "spot-agent",
                    "Spotledger Agent",
                    ServiceState::Running,
                    StartType::Automatic,
                ),
                (
                    "spot-runtime",
                    "Spotledger Runtime",
                    ServiceState::Running,
                    StartType::Automatic,
                ),
                (
                    "wuauserv",
                    "Windows Update",
                    ServiceState::Running,
                    StartType::Manual,
                ),
            ] {
                state.services.insert(
                    name.to_owned(),
                    ServiceInfo {
                        name: name.to_owned(),
                        display_name: display.to_owned(),
                        state: service_state,
                        start_type,
                    },
                );
                state.recovery.insert(name.to_owned(), no_recovery());
            }
        }
        fake
    }

    /// A machine with only the stock updates.
    #[must_use]
    pub fn with_stock_updates() -> Self {
        let fake = Self::new();
        fake.with_updates_defaults();
        fake
    }

    /// A machine with only the stock firewall rules and profiles.
    #[must_use]
    pub fn with_stock_firewall() -> Self {
        let fake = Self::new();
        fake.with_firewall_defaults();
        fake
    }

    /// A machine with only the stock local users and groups.
    #[must_use]
    pub fn with_stock_users() -> Self {
        let fake = Self::new();
        fake.with_users_defaults();
        fake
    }

    /// Adds the stock updates to an existing fake.
    fn with_updates_defaults(&self) {
        let mut state = self.lock();
        for update in [
            UpdateInfo {
                id: "u-critical".to_owned(),
                kb: Some("KB5034441".to_owned()),
                title: "Security Update".to_owned(),
                severity: Severity::Critical,
                reboot_required: true,
                downloaded: false,
                installed: false,
            },
            UpdateInfo {
                id: "u-important".to_owned(),
                kb: Some("KB5034123".to_owned()),
                title: "Cumulative Update".to_owned(),
                severity: Severity::Important,
                reboot_required: false,
                downloaded: true,
                installed: true,
            },
            UpdateInfo {
                id: "u-moderate".to_owned(),
                kb: Some("KB5030211".to_owned()),
                title: "Servicing Stack Update".to_owned(),
                severity: Severity::Moderate,
                reboot_required: false,
                downloaded: false,
                installed: false,
            },
            UpdateInfo {
                id: "u-low".to_owned(),
                kb: None,
                title: "Definition Update".to_owned(),
                severity: Severity::Low,
                reboot_required: false,
                downloaded: false,
                installed: false,
            },
        ] {
            state.updates.insert(update.id.clone(), update);
        }
    }

    /// Adds the stock firewall rules and profiles to an existing fake.
    fn with_firewall_defaults(&self) {
        let mut state = self.lock();
        for profile in Profile::all() {
            state.profiles.insert(profile, true);
        }
        for rule in [
            FirewallRule {
                name: "Web (HTTP-In)".to_owned(),
                direction: Direction::Inbound,
                action: RuleAction::Allow,
                protocol: Protocol::Tcp,
                local_ports: vec![80, 443],
                profiles: vec![Profile::Domain, Profile::Private, Profile::Public],
                enabled: true,
            },
            FirewallRule {
                name: "Remote Desktop - User Mode (TCP-In)".to_owned(),
                direction: Direction::Inbound,
                action: RuleAction::Allow,
                protocol: Protocol::Tcp,
                local_ports: vec![3389],
                profiles: vec![Profile::Domain, Profile::Private, Profile::Public],
                enabled: true,
            },
            FirewallRule {
                name: "Spotledger Agent (outbound)".to_owned(),
                direction: Direction::Outbound,
                action: RuleAction::Allow,
                protocol: Protocol::Tcp,
                local_ports: Vec::new(),
                profiles: vec![Profile::Domain, Profile::Private, Profile::Public],
                enabled: true,
            },
        ] {
            state.rules.insert(rule.name.clone(), rule);
        }
    }

    /// Adds the stock local users and groups to an existing fake.
    fn with_users_defaults(&self) {
        let mut state = self.lock();
        state.users.insert(
            "Administrator".to_owned(),
            LocalUser {
                name: "Administrator".to_owned(),
                enabled: true,
                full_name: "Built-in administrator".to_owned(),
                comment: String::new(),
                groups: vec!["Administrators".to_owned()],
            },
        );
        state.users.insert(
            "ana".to_owned(),
            LocalUser {
                name: "ana".to_owned(),
                enabled: true,
                full_name: "Ana".to_owned(),
                comment: String::new(),
                groups: vec!["Users".to_owned()],
            },
        );
        state.groups.insert(
            "Administrators".to_owned(),
            LocalGroup {
                name: "Administrators".to_owned(),
                comment: String::new(),
                members: vec!["Administrator".to_owned()],
            },
        );
        state.groups.insert(
            "Users".to_owned(),
            LocalGroup {
                name: "Users".to_owned(),
                comment: String::new(),
                members: vec!["ana".to_owned()],
            },
        );
    }

    /// The recorded length of the last password installed for `name`, if any. The value
    /// itself is never stored.
    #[must_use]
    pub fn recorded_password_len(&self, name: &str) -> Option<usize> {
        self.lock().password_lengths.get(name).copied()
    }

    /// Sets the reboot-required flag directly, for a test of the reported state.
    pub fn set_reboot_required(&self, required: bool) {
        self.lock().reboot_required = required;
    }

    /// Locks the state, recovering from a poisoned lock (a test that panicked mid-call
    /// should still see a consistent machine).
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for FakeWin {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FakeWin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("FakeWin")
            .field("services", &state.services.len())
            .field("updates", &state.updates.len())
            .field("rules", &state.rules.len())
            .field("users", &state.users.len())
            .field("groups", &state.groups.len())
            .finish()
    }
}

/// The recovery policy every service starts with: no action.
fn no_recovery() -> RecoveryConfig {
    RecoveryConfig {
        actions: vec![
            RecoveryAction::None,
            RecoveryAction::None,
            RecoveryAction::None,
        ],
        restart_delay_seconds: 0,
        reset_period_seconds: 86_400,
    }
}

impl WinApi for FakeWin {
    fn list_services(&self) -> Result<Vec<ServiceInfo>, WindowsError> {
        Ok(self.lock().services.values().cloned().collect())
    }

    fn service_status(&self, name: &str) -> Result<ServiceInfo, WindowsError> {
        self.lock()
            .services
            .get(name)
            .cloned()
            .ok_or_else(|| WindowsError::UnknownService {
                name: name.to_owned(),
            })
    }

    fn start_service(&self, name: &str, _timeout_seconds: u64) -> Result<(), WindowsError> {
        let mut state = self.lock();
        let service = state
            .services
            .get_mut(name)
            .ok_or_else(|| WindowsError::UnknownService {
                name: name.to_owned(),
            })?;
        service.state = ServiceState::Running;
        Ok(())
    }

    fn stop_service(&self, name: &str, _timeout_seconds: u64) -> Result<(), WindowsError> {
        let mut state = self.lock();
        let service = state
            .services
            .get_mut(name)
            .ok_or_else(|| WindowsError::UnknownService {
                name: name.to_owned(),
            })?;
        service.state = ServiceState::Stopped;
        Ok(())
    }

    fn set_service_start_type(
        &self,
        name: &str,
        start_type: StartType,
    ) -> Result<(), WindowsError> {
        let mut state = self.lock();
        let service = state
            .services
            .get_mut(name)
            .ok_or_else(|| WindowsError::UnknownService {
                name: name.to_owned(),
            })?;
        service.start_type = start_type;
        Ok(())
    }

    fn service_recovery(&self, name: &str) -> Result<RecoveryConfig, WindowsError> {
        let state = self.lock();
        if !state.services.contains_key(name) {
            return Err(WindowsError::UnknownService {
                name: name.to_owned(),
            });
        }
        Ok(state
            .recovery
            .get(name)
            .cloned()
            .unwrap_or_else(no_recovery))
    }

    fn set_service_recovery(
        &self,
        name: &str,
        recovery: &RecoveryConfig,
    ) -> Result<(), WindowsError> {
        let mut state = self.lock();
        if !state.services.contains_key(name) {
            return Err(WindowsError::UnknownService {
                name: name.to_owned(),
            });
        }
        state.recovery.insert(name.to_owned(), recovery.clone());
        Ok(())
    }

    fn search_updates(&self, _include_hidden: bool) -> Result<Vec<UpdateInfo>, WindowsError> {
        Ok(self.lock().updates.values().cloned().collect())
    }

    fn download_update(&self, id: &str) -> Result<(), WindowsError> {
        let mut state = self.lock();
        let update = state
            .updates
            .get_mut(id)
            .ok_or_else(|| WindowsError::UnknownUpdate { id: id.to_owned() })?;
        update.downloaded = true;
        Ok(())
    }

    fn install_update(&self, id: &str) -> Result<(), WindowsError> {
        let mut state = self.lock();
        let update = state
            .updates
            .get_mut(id)
            .ok_or_else(|| WindowsError::UnknownUpdate { id: id.to_owned() })?;
        update.installed = true;
        if update.reboot_required {
            state.reboot_required = true;
        }
        Ok(())
    }

    fn reboot_required(&self) -> Result<bool, WindowsError> {
        Ok(self.lock().reboot_required)
    }

    fn schedule_reboot(&self, at_unix_seconds: i64) -> Result<(), WindowsError> {
        if at_unix_seconds < 1 {
            return Err(WindowsError::RebootTimeInvalid {
                at: at_unix_seconds,
            });
        }
        self.lock().scheduled_reboot = Some(at_unix_seconds);
        Ok(())
    }

    fn scheduled_reboot(&self) -> Result<Option<i64>, WindowsError> {
        Ok(self.lock().scheduled_reboot)
    }

    fn list_firewall_rules(&self) -> Result<Vec<FirewallRule>, WindowsError> {
        Ok(self.lock().rules.values().cloned().collect())
    }

    fn add_firewall_rule(&self, rule: &FirewallRule) -> Result<(), WindowsError> {
        self.lock().rules.insert(rule.name.clone(), rule.clone());
        Ok(())
    }

    fn remove_firewall_rule(&self, name: &str) -> Result<(), WindowsError> {
        if self.lock().rules.remove(name).is_none() {
            return Err(WindowsError::UnknownRule {
                name: name.to_owned(),
            });
        }
        Ok(())
    }

    fn set_profile_enabled(&self, profile: Profile, enabled: bool) -> Result<(), WindowsError> {
        self.lock().profiles.insert(profile, enabled);
        Ok(())
    }

    fn profile_enabled(&self, profile: Profile) -> Result<bool, WindowsError> {
        Ok(*self.lock().profiles.get(&profile).unwrap_or(&true))
    }

    fn list_local_users(&self) -> Result<Vec<LocalUser>, WindowsError> {
        Ok(self.lock().users.values().cloned().collect())
    }

    fn local_user(&self, name: &str) -> Result<LocalUser, WindowsError> {
        self.lock()
            .users
            .get(name)
            .cloned()
            .ok_or_else(|| WindowsError::UnknownUser {
                name: name.to_owned(),
            })
    }

    fn create_local_user(
        &self,
        name: &str,
        password: &Secret,
        full_name: &str,
        comment: &str,
    ) -> Result<(), WindowsError> {
        let mut state = self.lock();
        state
            .password_lengths
            .insert(name.to_owned(), password.len());
        state.users.insert(
            name.to_owned(),
            LocalUser {
                name: name.to_owned(),
                enabled: true,
                full_name: full_name.to_owned(),
                comment: comment.to_owned(),
                groups: Vec::new(),
            },
        );
        Ok(())
    }

    fn set_user_enabled(&self, name: &str, enabled: bool) -> Result<(), WindowsError> {
        let mut state = self.lock();
        let user = state
            .users
            .get_mut(name)
            .ok_or_else(|| WindowsError::UnknownUser {
                name: name.to_owned(),
            })?;
        user.enabled = enabled;
        Ok(())
    }

    fn set_user_password(&self, name: &str, password: &Secret) -> Result<(), WindowsError> {
        let mut state = self.lock();
        if !state.users.contains_key(name) {
            return Err(WindowsError::UnknownUser {
                name: name.to_owned(),
            });
        }
        state
            .password_lengths
            .insert(name.to_owned(), password.len());
        Ok(())
    }

    fn set_user_groups(&self, name: &str, groups: &[String]) -> Result<(), WindowsError> {
        let mut state = self.lock();
        if !state.users.contains_key(name) {
            return Err(WindowsError::UnknownUser {
                name: name.to_owned(),
            });
        }
        for group in groups {
            if !state.groups.contains_key(group) {
                return Err(WindowsError::UnknownGroup {
                    name: group.clone(),
                });
            }
        }
        // Remove the user from every group, then add it to the requested ones, so the two
        // sides of the model stay consistent.
        for group in state.groups.values_mut() {
            group.members.retain(|member| member != name);
        }
        for group in groups {
            if let Some(entry) = state.groups.get_mut(group) {
                entry.members.push(name.to_owned());
            }
        }
        if let Some(user) = state.users.get_mut(name) {
            user.groups = groups.to_vec();
        }
        Ok(())
    }

    fn list_local_groups(&self) -> Result<Vec<LocalGroup>, WindowsError> {
        Ok(self.lock().groups.values().cloned().collect())
    }

    fn add_group_member(&self, group: &str, user: &str) -> Result<(), WindowsError> {
        let mut state = self.lock();
        if !state.groups.contains_key(group) {
            return Err(WindowsError::UnknownGroup {
                name: group.to_owned(),
            });
        }
        if !state.users.contains_key(user) {
            return Err(WindowsError::UnknownUser {
                name: user.to_owned(),
            });
        }
        if let Some(entry) = state.groups.get_mut(group)
            && !entry.members.iter().any(|member| member == user)
        {
            entry.members.push(user.to_owned());
        }
        if let Some(account) = state.users.get_mut(user)
            && !account.groups.iter().any(|member| member == group)
        {
            account.groups.push(group.to_owned());
        }
        Ok(())
    }

    fn remove_group_member(&self, group: &str, user: &str) -> Result<(), WindowsError> {
        let mut state = self.lock();
        if !state.groups.contains_key(group) {
            return Err(WindowsError::UnknownGroup {
                name: group.to_owned(),
            });
        }
        if !state.users.contains_key(user) {
            return Err(WindowsError::UnknownUser {
                name: user.to_owned(),
            });
        }
        if let Some(entry) = state.groups.get_mut(group) {
            entry.members.retain(|member| member != user);
        }
        if let Some(account) = state.users.get_mut(user) {
            account.groups.retain(|member| member != group);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn starting_and_stopping_change_the_state() {
        let fake = FakeWin::with_stock_services();
        assert_eq!(
            fake.service_status("spooler").unwrap().state,
            ServiceState::Stopped
        );
        fake.start_service("spooler", 30).unwrap();
        assert_eq!(
            fake.service_status("spooler").unwrap().state,
            ServiceState::Running
        );
        fake.stop_service("spooler", 30).unwrap();
        assert_eq!(
            fake.service_status("spooler").unwrap().state,
            ServiceState::Stopped
        );
    }

    #[test]
    fn a_password_is_recorded_only_by_length() {
        let fake = FakeWin::with_stock_users();
        let secret = Secret::new("Fixed-Passw0rd!Fixed-Pass");
        fake.set_user_password("ana", &secret).unwrap();
        assert_eq!(fake.recorded_password_len("ana"), Some(secret.len()));
    }

    #[test]
    fn unknown_names_are_refused() {
        let fake = FakeWin::with_stock_users();
        assert_eq!(
            fake.local_user("nobody").unwrap_err().code(),
            crate::error::UNKNOWN_USER
        );
        assert_eq!(
            fake.service_status("nothing").unwrap_err().code(),
            crate::error::UNKNOWN_SERVICE
        );
    }
}
