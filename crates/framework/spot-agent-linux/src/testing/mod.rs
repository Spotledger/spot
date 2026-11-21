//! A recording, in-memory fake of the Linux host the modules run against.
//!
//! [`FakeSystem`] implements both seams ([`CommandRunner`] and [`FileSystem`]) over a small
//! model of a distribution ([`model::State`]). A test constructs one (`FakeSystem::fedora()`,
//! `rhel()`, `ubuntu()`), hands its `runner()` and `filesystem()` to the modules, and asserts
//! on the commands that ran and the state that resulted. The golden transcripts
//! (`tests/transcripts/…`) seed the same state and record the expected command sequences.
//!
//! # The command contract
//!
//! The fake answers exactly the commands the modules emit; anything else is a spawn error, so a
//! test catches a module that reaches for a tool the fake does not model. The contract is:
//!
//! - **Packages** — `rpm -qa --queryformat …` / `rpm -q --queryformat … <pkgs>`,
//!   `dnf -q repolist` / `-q check-update` / `-q needs-restarting -r` / `-y install|remove|upgrade [--security]`,
//!   `dpkg-query -W -f … [<pkgs>]`, `apt-cache policy`, `apt-get -s upgrade` / `-y update|install|remove|upgrade`,
//!   `unattended-upgrade`, `fuser -s <lock>`.
//! - **Services** — `systemctl --no-pager show|start|stop|restart|enable|disable|mask|is-active|list-units`.
//! - **Users** — `useradd`, `usermod`, `userdel`, `gpasswd`, `id -nG`.
//! - **Firewall** — `firewall-cmd`, `ufw`, `nft`.

pub mod model;

mod commands;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::command::{CommandOutput, CommandRequest, CommandRunner, FileSystem};
use crate::error::LinuxError;

pub use model::{Distro, FirewallBackend, GroupState, ServiceState, State, Update, UserState};

/// The path of the dnf lock sentinel.
pub const DNF_LOCK: &str = "/var/run/dnf.pid";
/// The path of the apt lock frontend.
pub const APT_LOCK: &str = "/var/lib/dpkg/lock-frontend";
/// The path of the reboot-required marker on Debian-family hosts.
pub const REBOOT_REQUIRED: &str = "/var/run/reboot-required";

/// An in-memory host: a mutable [`State`] plus the commands that were run against it.
#[derive(Clone)]
pub struct FakeSystem {
    state: Arc<Mutex<State>>,
    recorded: Arc<Mutex<Vec<CommandRequest>>>,
}

impl FakeSystem {
    /// A Fedora host.
    #[must_use]
    pub fn fedora() -> Self {
        Self::from_state(State::fedora())
    }

    /// A RHEL-compatible host.
    #[must_use]
    pub fn rhel() -> Self {
        Self::from_state(State::rhel())
    }

    /// An Ubuntu host.
    #[must_use]
    pub fn ubuntu() -> Self {
        Self::from_state(State::ubuntu())
    }

    /// A host from an explicit state.
    #[must_use]
    pub fn from_state(state: State) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            recorded: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The command seam over this host.
    #[must_use]
    pub fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::new(self.clone())
    }

    /// The filesystem seam over this host.
    #[must_use]
    pub fn filesystem(&self) -> Arc<dyn FileSystem> {
        Arc::new(self.clone())
    }

    /// Runs `mutate` against the state and returns its result.
    pub fn update<R>(&self, mutate: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self.lock_state();
        mutate(&mut state)
    }

    /// The state lock, recovering the guard if another test panicked while holding it (a
    /// poisoned fake is still a usable fake).
    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The recorded-command lock, recovered the same way.
    fn lock_recorded(&self) -> std::sync::MutexGuard<'_, Vec<CommandRequest>> {
        self.recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A snapshot of the state.
    #[must_use]
    pub fn snapshot(&self) -> State {
        self.update(|state| state.clone())
    }

    /// The commands run so far, in order.
    #[must_use]
    pub fn commands(&self) -> Vec<CommandRequest> {
        self.lock_recorded().clone()
    }

    /// Forgets the recorded commands.
    pub fn clear_commands(&self) {
        self.lock_recorded().clear();
    }

    /// Records that `name` is installed.
    pub fn mark_installed(&self, name: &str) {
        self.update(|state| {
            state.installed.insert(name.to_owned(), "1.0".to_owned());
        });
    }

    /// Records that `name` is not installed.
    pub fn mark_removed(&self, name: &str) {
        self.update(|state| {
            state.installed.remove(name);
        });
    }

    /// Adds one available update.
    pub fn add_update(&self, name: &str, security: bool) {
        self.update(|state| {
            state.updates.push(Update {
                name: name.to_owned(),
                security,
            });
        });
    }

    /// Sets whether a reboot is required.
    pub fn set_reboot_required(&self, required: bool) {
        self.update(|state| state.reboot_required = required);
    }

    /// Sets whether the package manager lock is held.
    pub fn set_package_locked(&self, locked: bool) {
        self.update(|state| state.package_locked = locked);
    }

    /// Adds an account at `uid` with a matching primary group.
    pub fn add_user(&self, name: &str, uid: u32) {
        self.update(|state| {
            state.users.insert(
                name.to_owned(),
                UserState {
                    uid,
                    gid: uid,
                    home: format!("/home/{name}"),
                    shell: "/bin/bash".to_owned(),
                    locked: false,
                    groups: std::collections::BTreeSet::new(),
                },
            );
            state.groups.entry(name.to_owned()).or_insert(GroupState {
                gid: uid,
                members: std::collections::BTreeSet::new(),
            });
        });
    }

    /// Sets a unit's active state.
    pub fn set_service_active(&self, unit: &str, active: bool) {
        self.update(|state| {
            state
                .services
                .entry(unit.to_owned())
                .or_insert(ServiceState {
                    active: false,
                    enabled: false,
                    masked: false,
                    failed: false,
                })
                .active = active;
        });
    }

    /// Marks a unit as failed.
    pub fn set_service_failed(&self, unit: &str, failed: bool) {
        self.update(|state| {
            state
                .services
                .entry(unit.to_owned())
                .or_insert(ServiceState {
                    active: false,
                    enabled: false,
                    masked: false,
                    failed: false,
                })
                .failed = failed;
        });
    }

    /// The active firewall backend.
    #[must_use]
    pub fn firewall_backend(&self) -> FirewallBackend {
        self.update(|state| state.firewall)
    }
}

impl CommandRunner for FakeSystem {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput, LinuxError> {
        self.lock_recorded().push(request.clone());
        let mut state = self.lock_state();
        let args = &request.args;
        match request.program.as_str() {
            "rpm" => commands::rpm(&state, args),
            "dnf" => commands::dnf(&mut state, args),
            "dpkg-query" => commands::dpkg_query(&state, args),
            "apt-cache" => commands::apt_cache(&state, args),
            "apt-get" => commands::apt_get(&mut state, args),
            "unattended-upgrade" => commands::unattended_upgrade(&mut state, args),
            "fuser" => commands::fuser(&state, args),
            "systemctl" => commands::systemctl(&mut state, args),
            "useradd" => commands::useradd(&mut state, args),
            "usermod" => commands::usermod(&mut state, args),
            "userdel" => commands::userdel(&mut state, args),
            "gpasswd" => commands::gpasswd(&mut state, args),
            "id" => commands::id(&state, args),
            "firewall-cmd" => commands::firewall_cmd(&mut state, args),
            "ufw" => commands::ufw(&mut state, args),
            "nft" => commands::nft(&mut state, args),
            other => Err(LinuxError::CommandSpawn {
                program: other.to_owned(),
                detail: "the fake does not model this program".to_owned(),
            }),
        }
    }
}

impl FileSystem for FakeSystem {
    fn read_to_string(&self, path: &str) -> Result<String, LinuxError> {
        let state = self.lock_state();
        match path {
            "/etc/os-release" => Ok(state.distro.os_release()),
            "/etc/passwd" => Ok(state.passwd()),
            "/etc/group" => Ok(state.group_file()),
            "/etc/shadow" => Ok(state.shadow()),
            REBOOT_REQUIRED if state.reboot_required => {
                Ok("*** System restart required ***\n".to_owned())
            }
            other => Err(LinuxError::Io {
                path: other.to_owned(),
                detail: "the fake has no such file".to_owned(),
            }),
        }
    }

    fn exists(&self, path: &str) -> Result<bool, LinuxError> {
        let state = self.lock_state();
        let exists = match path {
            REBOOT_REQUIRED => state.reboot_required,
            DNF_LOCK => state.package_locked && state.distro.backend() == "dnf",
            APT_LOCK => state.package_locked && state.distro.backend() == "apt",
            _ => false,
        };
        Ok(exists)
    }
}

impl std::fmt::Debug for FakeSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeSystem")
            .field("distro", &self.update(|state| state.distro))
            .field("commands", &self.commands().len())
            .finish()
    }
}

/// A convenience: the state's installed package names, for assertions.
#[must_use]
pub fn installed_names(state: &State) -> Vec<String> {
    state.installed.keys().cloned().collect()
}

/// A convenience: the state's services by name, for assertions.
#[must_use]
pub fn services(state: &State) -> &BTreeMap<String, ServiceState> {
    &state.services
}
