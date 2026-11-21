//! The mutable state the fake command runner simulates, and its per-distribution seeds.
//!
//! This is a small model of the parts of a Linux host the modules touch: the installed
//! packages, the available updates, the systemd units, the accounts and groups, and the
//! firewall. [`crate::testing::FakeSystem`] turns commands into changes on this state and
//! renders the state back as the output the real tools would print.

use std::collections::{BTreeMap, BTreeSet};

/// A distribution family the golden transcripts cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Distro {
    /// Fedora (development), package backend `dnf`.
    Fedora,
    /// A RHEL-compatible distribution, package backend `dnf`.
    Rhel,
    /// Ubuntu LTS, package backend `apt`.
    Ubuntu,
}

impl Distro {
    /// The `ID` line of `/etc/os-release`.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Fedora => "fedora",
            Self::Rhel => "rhel",
            Self::Ubuntu => "ubuntu",
        }
    }

    /// The package backend's short name.
    #[must_use]
    pub fn backend(self) -> &'static str {
        match self {
            Self::Fedora | Self::Rhel => "dnf",
            Self::Ubuntu => "apt",
        }
    }

    /// The `/etc/os-release` contents this distribution reports.
    #[must_use]
    pub fn os_release(self) -> String {
        match self {
            Self::Fedora => concat!(
                "NAME=\"Fedora Linux\"\n",
                "ID=fedora\n",
                "VERSION_ID=41\n",
                "PRETTY_NAME=\"Fedora Linux 41 (Workstation Edition)\"\n"
            )
            .to_owned(),
            Self::Rhel => concat!(
                "NAME=\"Red Hat Enterprise Linux\"\n",
                "ID=\"rhel\"\n",
                "ID_LIKE=\"fedora\"\n",
                "VERSION_ID=\"9.4\"\n",
                "PRETTY_NAME=\"Red Hat Enterprise Linux 9.4 (Plow)\"\n"
            )
            .to_owned(),
            Self::Ubuntu => concat!(
                "NAME=\"Ubuntu\"\n",
                "ID=ubuntu\n",
                "ID_LIKE=debian\n",
                "VERSION_ID=\"24.04\"\n",
                "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\n"
            )
            .to_owned(),
        }
    }

    /// The SSH service unit name this family uses.
    #[must_use]
    pub fn ssh_service(self) -> &'static str {
        match self {
            Self::Ubuntu => "ssh.service",
            Self::Fedora | Self::Rhel => "sshd.service",
        }
    }
}

/// One available package update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// The package name.
    pub name: String,
    /// Whether the update comes from a security source.
    pub security: bool,
}

/// The state of one systemd unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceState {
    /// Whether the unit is active (`ActiveState=active`).
    pub active: bool,
    /// Whether the unit is enabled at boot (`UnitFileState=enabled`).
    pub enabled: bool,
    /// Whether the unit is masked.
    pub masked: bool,
    /// Whether the unit is in the `failed` state.
    pub failed: bool,
}

/// The state of one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserState {
    /// The numeric UID.
    pub uid: u32,
    /// The primary GID.
    pub gid: u32,
    /// The home directory.
    pub home: String,
    /// The login shell.
    pub shell: String,
    /// Whether the password is locked (`!` in the shadow file, modelled here as a flag).
    pub locked: bool,
    /// The supplementary groups the account belongs to (by name).
    pub groups: BTreeSet<String>,
}

/// The state of one group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupState {
    /// The numeric GID.
    pub gid: u32,
    /// The member account names.
    pub members: BTreeSet<String>,
}

/// The active firewall backend the fake reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirewallBackend {
    /// firewalld (`firewall-cmd`).
    Firewalld,
    /// ufw.
    Ufw,
    /// nftables (`nft`).
    Nftables,
}

/// The whole simulated host.
#[derive(Clone, Debug)]
pub struct State {
    /// The distribution family.
    pub distro: Distro,
    /// Installed packages, name to version.
    pub installed: BTreeMap<String, String>,
    /// Available updates.
    pub updates: Vec<Update>,
    /// Whether the host reports a reboot is required.
    pub reboot_required: bool,
    /// Whether the package manager is busy (its lock is held).
    pub package_locked: bool,
    /// The systemd units.
    pub services: BTreeMap<String, ServiceState>,
    /// The accounts.
    pub users: BTreeMap<String, UserState>,
    /// The groups.
    pub groups: BTreeMap<String, GroupState>,
    /// The active firewall backend.
    pub firewall: FirewallBackend,
    /// Whether the firewall backend is active.
    pub firewall_active: bool,
    /// Open firewall ports, as `port/protocol`.
    pub firewall_ports: BTreeSet<String>,
    /// Allowed firewall sources (CIDRs).
    pub firewall_sources: BTreeSet<String>,
    /// The next free UID for `useradd`.
    pub next_uid: u32,
    /// The next free GID.
    pub next_gid: u32,
    /// The nftables rule handle per expression key.
    pub nft_handles: BTreeMap<String, u32>,
    /// The next free nftables handle.
    pub next_handle: u32,
}

impl State {
    /// A Fedora host with an sshd unit, firewalld and a spot agent.
    #[must_use]
    pub fn fedora() -> Self {
        Self::rhel_like(Distro::Fedora, FirewallBackend::Firewalld)
    }

    /// A RHEL-compatible host, firewalld.
    #[must_use]
    pub fn rhel() -> Self {
        Self::rhel_like(Distro::Rhel, FirewallBackend::Firewalld)
    }

    /// An Ubuntu host with ufw.
    #[must_use]
    pub fn ubuntu() -> Self {
        Self::ubuntu_like(FirewallBackend::Ufw)
    }

    fn rhel_like(distro: Distro, firewall: FirewallBackend) -> Self {
        let mut state = Self::base(distro, firewall);
        state.services.insert(
            "sshd.service".to_owned(),
            ServiceState {
                active: true,
                enabled: true,
                masked: false,
                failed: false,
            },
        );
        state
    }

    fn ubuntu_like(firewall: FirewallBackend) -> Self {
        let mut state = Self::base(Distro::Ubuntu, firewall);
        state.services.insert(
            "ssh.service".to_owned(),
            ServiceState {
                active: true,
                enabled: true,
                masked: false,
                failed: false,
            },
        );
        state
    }

    fn base(distro: Distro, firewall: FirewallBackend) -> Self {
        let mut services = BTreeMap::new();
        for unit in [
            "spot-agent.service",
            "spot-runtime.service",
            "crond.service",
            "nginx.service",
        ] {
            services.insert(
                unit.to_owned(),
                ServiceState {
                    active: unit == "spot-agent.service" || unit == "spot-runtime.service",
                    enabled: unit == "spot-agent.service" || unit == "spot-runtime.service",
                    masked: false,
                    failed: false,
                },
            );
        }
        let mut users = BTreeMap::new();
        users.insert("root".to_owned(), system_user(0, 0, "/root", "/bin/bash"));
        users.insert(
            "spot".to_owned(),
            system_user(990, 990, "/var/lib/spot", "/sbin/nologin"),
        );
        let mut groups = BTreeMap::new();
        groups.insert(
            "root".to_owned(),
            GroupState {
                gid: 0,
                members: BTreeSet::new(),
            },
        );
        groups.insert(
            "spot".to_owned(),
            GroupState {
                gid: 990,
                members: ["spot".to_owned()].into_iter().collect(),
            },
        );
        let admin_group = if distro == Distro::Ubuntu {
            "sudo"
        } else {
            "wheel"
        };
        groups.insert(
            admin_group.to_owned(),
            GroupState {
                gid: if distro == Distro::Ubuntu { 27 } else { 10 },
                members: BTreeSet::new(),
            },
        );
        let mut firewall_ports = BTreeSet::new();
        firewall_ports.insert("22/tcp".to_owned());
        Self {
            distro,
            installed: [
                ("bash", "5.2.26"),
                ("systemd", "255"),
                ("openssh-server", "9.6"),
                ("firewalld", "2.1"),
            ]
            .into_iter()
            .map(|(name, version)| (name.to_owned(), version.to_owned()))
            .collect(),
            updates: Vec::new(),
            reboot_required: false,
            package_locked: false,
            services,
            users,
            groups,
            firewall,
            firewall_active: true,
            firewall_ports,
            firewall_sources: BTreeSet::new(),
            next_uid: 1000,
            next_gid: 1000,
            nft_handles: BTreeMap::new(),
            next_handle: 1,
        }
    }

    /// Whether `name` is installed.
    #[must_use]
    pub fn is_installed(&self, name: &str) -> bool {
        self.installed.contains_key(name)
    }

    /// Renders `/etc/passwd`, one account per line, sorted by UID.
    #[must_use]
    pub fn passwd(&self) -> String {
        let mut accounts: Vec<(&String, &UserState)> = self.users.iter().collect();
        accounts.sort_by_key(|(_, user)| user.uid);
        let mut out = String::new();
        for (name, user) in accounts {
            out.push_str(&format!(
                "{name}:x:{}:{}:{}:{}:{}\n",
                user.uid, user.gid, name, user.home, user.shell
            ));
        }
        out
    }

    /// Renders `/etc/group`, one group per line, sorted by GID.
    #[must_use]
    pub fn group_file(&self) -> String {
        let mut groups: Vec<(&String, &GroupState)> = self.groups.iter().collect();
        groups.sort_by_key(|(_, group)| group.gid);
        let mut out = String::new();
        for (name, group) in groups {
            let members: Vec<&str> = group.members.iter().map(String::as_str).collect();
            out.push_str(&format!("{name}:x:{}:{}\n", group.gid, members.join(",")));
        }
        out
    }

    /// The UID of `name`, when the account exists.
    #[must_use]
    pub fn uid_of(&self, name: &str) -> Option<u32> {
        self.users.get(name).map(|user| user.uid)
    }

    /// Renders `/etc/shadow`, one account per line. A locked account's password field starts
    /// with `!`, as the real `usermod --lock` writes it.
    #[must_use]
    pub fn shadow(&self) -> String {
        let mut accounts: Vec<(&String, &UserState)> = self.users.iter().collect();
        accounts.sort_by_key(|(_, user)| user.uid);
        let mut out = String::new();
        for (name, user) in accounts {
            let password = if user.locked {
                "!"
            } else {
                "$6$spotagent$abcdefghijklmnop"
            };
            out.push_str(&format!("{name}:{password}:19000:0:99999:7:::\n"));
        }
        out
    }
}

/// A system account below the UID floor.
fn system_user(uid: u32, gid: u32, home: &str, shell: &str) -> UserState {
    UserState {
        uid,
        gid,
        home: home.to_owned(),
        shell: shell.to_owned(),
        locked: false,
        groups: BTreeSet::new(),
    }
}
