//! The host context every Linux module shares: the two seams and the deployment's facts.
//!
//! A module is constructed from a [`LinuxContext`] (usually behind an `Arc`). The context
//! carries the injected [`CommandRunner`] and [`FileSystem`], the agent's and runtime's own
//! unit names (which the service module refuses to stop or mask), the SSH and management ports
//! (which the firewall module guards against locking out) and the account UID floor.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::command::{CommandRunner, FileSystem};

/// The default UID floor: accounts below it are system accounts, never managed without an
/// explicit allow-list entry.
pub const DEFAULT_UID_FLOOR: u32 = 1000;
/// The default SSH service unit name (RHEL-family spelling; Ubuntu uses `ssh.service`).
pub const DEFAULT_SSH_SERVICE: &str = concat!("sshd", ".service");
/// The default SSH port.
pub const DEFAULT_SSH_PORT: u16 = 22;

/// The seams and deployment facts a Linux module needs.
#[derive(Clone)]
pub struct LinuxContext {
    /// The command seam.
    pub runner: Arc<dyn CommandRunner>,
    /// The filesystem seam.
    pub fs: Arc<dyn FileSystem>,
    /// The agent's own systemd unit: never stopped, restarted, disabled or masked without the
    /// critical flag.
    pub agent_unit: String,
    /// The Spotledger runtime's unit, protected the same way.
    pub runtime_unit: String,
    /// The SSH service unit, protected by the firewall lock-out guard.
    pub ssh_service: String,
    /// The SSH port, protected by the firewall lock-out guard.
    pub ssh_port: u16,
    /// The agent's management port, when it listens on one; also protected.
    pub management_port: Option<u16>,
    /// The UID floor: an account below it needs an allow-list entry to be touched.
    pub uid_floor: u32,
    /// The explicit allow-list of below-floor UIDs the deployment has agreed to manage.
    pub allowed_low_uids: BTreeSet<u32>,
}

impl LinuxContext {
    /// A context over the two seams, with the default deployment facts.
    #[must_use]
    pub fn new(runner: Arc<dyn CommandRunner>, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            runner,
            fs,
            agent_unit: "spot-agent.service".to_owned(),
            runtime_unit: "spot-runtime.service".to_owned(),
            ssh_service: DEFAULT_SSH_SERVICE.to_owned(),
            ssh_port: DEFAULT_SSH_PORT,
            management_port: None,
            uid_floor: DEFAULT_UID_FLOOR,
            allowed_low_uids: BTreeSet::new(),
        }
    }

    /// Sets the agent's own unit name.
    #[must_use]
    pub fn with_agent_unit(mut self, unit: impl Into<String>) -> Self {
        self.agent_unit = unit.into();
        self
    }

    /// Sets the runtime's unit name.
    #[must_use]
    pub fn with_runtime_unit(mut self, unit: impl Into<String>) -> Self {
        self.runtime_unit = unit.into();
        self
    }

    /// Sets the SSH service unit name.
    #[must_use]
    pub fn with_ssh_service(mut self, unit: impl Into<String>) -> Self {
        self.ssh_service = unit.into();
        self
    }

    /// Sets the SSH port.
    #[must_use]
    pub fn with_ssh_port(mut self, port: u16) -> Self {
        self.ssh_port = port;
        self
    }

    /// Sets the agent's management port.
    #[must_use]
    pub fn with_management_port(mut self, port: u16) -> Self {
        self.management_port = Some(port);
        self
    }

    /// Sets the UID floor.
    #[must_use]
    pub fn with_uid_floor(mut self, floor: u32) -> Self {
        self.uid_floor = floor;
        self
    }

    /// Allows one below-floor UID to be managed.
    #[must_use]
    pub fn allow_low_uid(mut self, uid: u32) -> Self {
        self.allowed_low_uids.insert(uid);
        self
    }

    /// Whether `unit` is one of the units the service module protects.
    #[must_use]
    pub fn is_protected_unit(&self, unit: &str) -> bool {
        unit == self.agent_unit || unit == self.runtime_unit
    }

    /// Whether `uid` may be touched: at or above the floor, or explicitly allow-listed.
    #[must_use]
    pub fn uid_is_manageable(&self, uid: u32) -> bool {
        uid >= self.uid_floor || self.allowed_low_uids.contains(&uid)
    }

    /// Whether `port` carries management access and so needs the critical flag to close:
    /// the SSH port, or the agent's management port.
    #[must_use]
    pub fn port_is_management(&self, port: u16) -> bool {
        port == self.ssh_port || self.management_port == Some(port)
    }
}

impl std::fmt::Debug for LinuxContext {
    /// Prints the deployment facts, never the seams' internals.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxContext")
            .field("agent_unit", &self.agent_unit)
            .field("runtime_unit", &self.runtime_unit)
            .field("ssh_service", &self.ssh_service)
            .field("ssh_port", &self.ssh_port)
            .field("management_port", &self.management_port)
            .field("uid_floor", &self.uid_floor)
            .field("allowed_low_uids", &self.allowed_low_uids)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testing::FakeSystem;

    fn context() -> LinuxContext {
        let system = FakeSystem::fedora();
        LinuxContext::new(system.runner(), system.filesystem())
    }

    #[test]
    fn protected_units_are_the_agents_and_the_runtimes() {
        let context = context();
        assert!(context.is_protected_unit("spot-agent.service"));
        assert!(context.is_protected_unit("spot-runtime.service"));
        assert!(!context.is_protected_unit(concat!("sshd", ".service")));
    }

    #[test]
    fn the_uid_floor_and_allow_list_decide_manageability() {
        let context = context();
        assert!(context.uid_is_manageable(1000));
        assert!(!context.uid_is_manageable(0));
        assert!(context.allow_low_uid(0).uid_is_manageable(0));
    }

    #[test]
    fn management_ports_cover_ssh_and_the_agent() {
        let context = context().with_management_port(8443);
        assert!(context.port_is_management(22));
        assert!(context.port_is_management(8443));
        assert!(!context.port_is_management(80));
    }
}
