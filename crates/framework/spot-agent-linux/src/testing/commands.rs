//! The command simulation: how the fake answers each program the modules run.
//!
//! Every function here takes the simulated [`State`] and the request's arguments and returns
//! the output the real tool would print, mutating the state for a command that changes
//! something. The contract with the modules is fixed and small; it is documented at the top of
//! [`crate::testing`].

use std::collections::BTreeSet;

use crate::command::CommandOutput;
use crate::error::LinuxError;

use super::model::{FirewallBackend, GroupState, ServiceState, State, UserState};

/// A successful output with `stdout`.
fn out(status: i32, stdout: impl Into<String>) -> CommandOutput {
    CommandOutput {
        status,
        stdout: stdout.into(),
        stderr: String::new(),
        timed_out: false,
        truncated: false,
    }
}

/// An output with `stderr`.
fn err(status: i32, stderr: impl Into<String>) -> CommandOutput {
    CommandOutput {
        status,
        stdout: String::new(),
        stderr: stderr.into(),
        timed_out: false,
        truncated: false,
    }
}

/// The repository listing a dnf host reports.
fn dnf_repos(state: &State) -> String {
    match state.distro {
        super::model::Distro::Fedora => concat!(
            "repo id              repo name\n",
            "fedora               Fedora 41 - x86_64\n",
            "updates              Fedora 41 - x86_64 - Updates\n"
        )
        .to_owned(),
        super::model::Distro::Rhel => concat!(
            "repo id                              repo name\n",
            "rhel-9-for-x86_64-baseos-rpms        Red Hat Enterprise Linux 9 BaseOS\n",
            "rhel-9-for-x86_64-appstream-rpms     Red Hat Enterprise Linux 9 AppStream\n"
        )
        .to_owned(),
        super::model::Distro::Ubuntu => "ubuntu-main  Ubuntu 24.04 main\n".to_owned(),
    }
}

/// Clears the available updates, reporting a reboot if a kernel or glibc package moved.
fn clear_updates(state: &mut State, security_only: bool) {
    let reboot = state.updates.iter().any(|update| {
        (!security_only || update.security)
            && (update.name.starts_with("kernel") || update.name.starts_with("glibc"))
    });
    if security_only {
        state.updates.retain(|update| !update.security);
    } else {
        state.updates.clear();
    }
    if reboot {
        state.reboot_required = true;
    }
}

/// `rpm -qa --queryformat …` (all) and `rpm -q --queryformat … <pkgs>` (a subset).
pub(super) fn rpm(state: &State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let list_all = args.first().map(String::as_str) == Some("-qa");
    let packages = packages_after(args, "--queryformat");
    if list_all {
        let names: Vec<&str> = state.installed.keys().map(String::as_str).collect();
        return Ok(out(0, names.join("\n")));
    }
    let mut stdout = String::new();
    let mut missing: Vec<&str> = Vec::new();
    for package in &packages {
        if state.is_installed(package) {
            stdout.push_str(package);
            stdout.push('\n');
        } else {
            missing.push(package);
        }
    }
    if missing.is_empty() {
        Ok(out(0, stdout))
    } else {
        let mut stderr = String::new();
        for name in missing {
            stderr.push_str(&format!("package {name} is not installed\n"));
        }
        Ok(CommandOutput {
            status: 1,
            stdout,
            stderr,
            timed_out: false,
            truncated: false,
        })
    }
}

/// The arguments after the value of the flag `flag` (used for `--queryformat` and `-f`).
fn packages_after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    match args.iter().position(|arg| arg == flag) {
        Some(position) => args[position + 1..]
            .iter()
            .skip(1)
            .map(String::as_str)
            .collect(),
        None => Vec::new(),
    }
}

/// `dnf` package and update operations.
pub(super) fn dnf(state: &mut State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let tokens: Vec<&str> = args.iter().map(String::as_str).collect();
    match tokens.as_slice() {
        ["-q", "repolist"] => Ok(out(0, dnf_repos(state))),
        ["-q", "check-update"] => {
            if state.updates.is_empty() {
                Ok(out(0, ""))
            } else {
                let mut stdout = String::new();
                for update in &state.updates {
                    stdout.push_str(&format!(
                        "{}.x86_64  {}-1  {}\n",
                        update.name,
                        update.name,
                        if update.security {
                            "security"
                        } else {
                            "updates"
                        }
                    ));
                }
                Ok(out(100, stdout))
            }
        }
        ["-q", "needs-restarting", "-r"] => {
            if state.reboot_required {
                Ok(err(
                    1,
                    "Core libraries or services have been updated.\nReboot is required to ensure that your system benefits from these updates.\n",
                ))
            } else {
                Ok(out(0, ""))
            }
        }
        ["-y", "install", packages @ ..] => {
            for package in packages {
                state
                    .installed
                    .insert((*package).to_owned(), "1.0-1".to_owned());
            }
            Ok(out(0, "Complete!"))
        }
        ["-y", "remove", packages @ ..] => {
            for package in packages {
                state.installed.remove(*package);
            }
            Ok(out(0, "Complete!"))
        }
        ["-y", "upgrade"] => {
            clear_updates(state, false);
            Ok(out(0, "Complete!"))
        }
        ["-y", "upgrade", "--security"] => {
            clear_updates(state, true);
            Ok(out(0, "Complete!"))
        }
        _ => Ok(err(1, "dnf: the fake does not model this invocation\n")),
    }
}

/// `dpkg-query -W -f …` (all) and `dpkg-query -W -f … <pkgs>` (a subset).
pub(super) fn dpkg_query(state: &State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let packages = packages_after(args, "-f");
    if packages.is_empty() {
        let names: Vec<&str> = state.installed.keys().map(String::as_str).collect();
        return Ok(out(0, names.join("\n")));
    }
    let mut stdout = String::new();
    let mut missing = false;
    for package in &packages {
        if state.is_installed(package) {
            stdout.push_str(&format!("ii {package}\n"));
        } else {
            stdout.push_str(&format!("un {package}\n"));
            missing = true;
        }
    }
    Ok(out(if missing { 1 } else { 0 }, stdout))
}

/// `apt-cache policy`.
pub(super) fn apt_cache(state: &State, _args: &[String]) -> Result<CommandOutput, LinuxError> {
    let _ = state;
    Ok(out(
        0,
        "Package files:\n 100 /var/lib/dpkg/status\n500 http://archive.ubuntu.com/ubuntu noble/main amd64 Packages\n",
    ))
}

/// `apt-get` package and update operations.
pub(super) fn apt_get(state: &mut State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let tokens: Vec<&str> = args.iter().map(String::as_str).collect();
    match tokens.as_slice() {
        ["-s", "upgrade"] => {
            let mut stdout = String::new();
            for update in &state.updates {
                stdout.push_str(&format!(
                    "Inst {} [1.0] (1.1 {} [amd64])\n",
                    update.name, update.name
                ));
            }
            Ok(out(0, stdout))
        }
        ["-y", "update"] => Ok(out(0, "Reading package lists... Done")),
        ["-y", "install", packages @ ..] => {
            for package in packages {
                state
                    .installed
                    .insert((*package).to_owned(), "1.1".to_owned());
            }
            Ok(out(0, "Setting up packages... Done"))
        }
        ["-y", "remove", packages @ ..] => {
            for package in packages {
                state.installed.remove(*package);
            }
            Ok(out(0, "Removing packages... Done"))
        }
        ["-y", "upgrade"] => {
            clear_updates(state, false);
            Ok(out(0, "Done"))
        }
        _ => Ok(err(1, "apt-get: the fake does not model this invocation\n")),
    }
}

/// `unattended-upgrade` (Ubuntu's security-update path). `--dry-run` lists what would move
/// without changing anything.
pub(super) fn unattended_upgrade(
    state: &mut State,
    args: &[String],
) -> Result<CommandOutput, LinuxError> {
    if args.iter().any(|arg| arg == "--dry-run") {
        let pending: Vec<&str> = state
            .updates
            .iter()
            .filter(|update| update.security)
            .map(|update| update.name.as_str())
            .collect();
        return Ok(out(0, pending.join("\n")));
    }
    clear_updates(state, true);
    Ok(out(0, "All upgrades installed"))
}

/// `fuser -s <lock>`: exit 0 when a process holds the lock.
pub(super) fn fuser(state: &State, _args: &[String]) -> Result<CommandOutput, LinuxError> {
    if state.package_locked {
        Ok(out(0, ""))
    } else {
        Ok(err(1, ""))
    }
}

/// Whether a unit is active in the simulated state.
fn unit_active(state: &State, unit: &str) -> bool {
    if unit.starts_with("firewalld") {
        return state.firewall == FirewallBackend::Firewalld && state.firewall_active;
    }
    if unit.starts_with("ufw") {
        return state.firewall == FirewallBackend::Ufw && state.firewall_active;
    }
    state
        .services
        .get(unit)
        .is_some_and(|service| service.active)
}

/// The service state of a unit, defaulting to inactive for an unknown unit.
fn service_state(state: &State, unit: &str) -> ServiceState {
    state.services.get(unit).cloned().unwrap_or(ServiceState {
        active: unit_active(state, unit),
        enabled: false,
        masked: false,
        failed: false,
    })
}

/// `systemctl` service management.
pub(super) fn systemctl(state: &mut State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let verb = args
        .iter()
        .map(String::as_str)
        .find(|arg| !arg.starts_with('-'));
    let unit = args
        .iter()
        .rev()
        .map(String::as_str)
        .find(|arg| !arg.starts_with('-'))
        .unwrap_or("");
    match verb {
        Some("is-active") => {
            if unit_active(state, unit) {
                Ok(out(0, "active"))
            } else {
                Ok(err(3, "inactive"))
            }
        }
        Some("show") => {
            let service = service_state(state, unit);
            let file_state = if service.masked {
                "masked"
            } else if service.enabled {
                "enabled"
            } else {
                "disabled"
            };
            Ok(out(
                0,
                format!(
                    "ActiveState={}\nSubState={}\nUnitFileState={}\n",
                    if service.active { "active" } else { "inactive" },
                    if service.active { "running" } else { "dead" },
                    file_state
                ),
            ))
        }
        Some("start") => {
            if let Some(service) = state.services.get_mut(unit) {
                service.active = true;
                service.failed = false;
            }
            Ok(out(0, ""))
        }
        Some("stop") => {
            if let Some(service) = state.services.get_mut(unit) {
                service.active = false;
            }
            Ok(out(0, ""))
        }
        Some("restart") => {
            if let Some(service) = state.services.get_mut(unit) {
                service.active = true;
                service.failed = false;
            }
            Ok(out(0, ""))
        }
        Some("enable") => {
            if let Some(service) = state.services.get_mut(unit) {
                service.enabled = true;
            }
            Ok(out(0, ""))
        }
        Some("disable") => {
            if let Some(service) = state.services.get_mut(unit) {
                service.enabled = false;
            }
            Ok(out(0, ""))
        }
        Some("mask") => {
            if let Some(service) = state.services.get_mut(unit) {
                service.masked = true;
                service.enabled = false;
                service.active = false;
            }
            Ok(out(0, ""))
        }
        Some("list-units") => {
            let failed: Vec<String> = state
                .services
                .iter()
                .filter(|(_, service)| service.failed)
                .map(|(name, _)| format!("{name} loaded failed failed"))
                .collect();
            Ok(out(0, failed.join("\n")))
        }
        _ => Ok(err(
            1,
            "systemctl: the fake does not model this invocation\n",
        )),
    }
}

/// The account name: the last argument that is not a flag.
fn trailing_name(args: &[String]) -> Option<String> {
    args.iter()
        .rev()
        .map(String::as_str)
        .find(|arg| !arg.starts_with('-'))
        .map(str::to_owned)
}

/// `useradd`.
pub(super) fn useradd(state: &mut State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let Some(name) = trailing_name(args) else {
        return Ok(err(2, "useradd: no user name\n"));
    };
    if state.users.contains_key(&name) {
        return Ok(err(9, format!("useradd: user '{name}' already exists\n")));
    }
    let uid = flag_value(args, "--uid")
        .and_then(|value| value.parse().ok())
        .unwrap_or(state.next_uid);
    let home = flag_value(args, "--home-dir").unwrap_or_else(|| format!("/home/{name}"));
    let shell = flag_value(args, "--shell").unwrap_or_else(|| "/bin/bash".to_owned());
    let groups: BTreeSet<String> = flag_value(args, "--groups")
        .map(|value| value.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let gid = state.next_gid;
    state.next_uid = state.next_uid.max(uid + 1);
    state.next_gid = state.next_gid.max(gid + 1);
    state.users.insert(
        name.clone(),
        UserState {
            uid,
            gid,
            home,
            shell,
            locked: false,
            groups: groups.clone(),
        },
    );
    state.groups.entry(name.clone()).or_insert(GroupState {
        gid,
        members: BTreeSet::new(),
    });
    for group in groups {
        state
            .groups
            .entry(group)
            .or_insert(GroupState {
                gid: state.next_gid,
                members: BTreeSet::new(),
            })
            .members
            .insert(name.clone());
    }
    Ok(out(0, ""))
}

/// The value of `--flag value` in `args`.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let position = args.iter().position(|arg| arg == flag)?;
    args.get(position + 1).cloned()
}

/// `usermod` (lock, unlock and group membership).
pub(super) fn usermod(state: &mut State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let Some(name) = trailing_name(args) else {
        return Ok(err(2, "usermod: no user name\n"));
    };
    if args.iter().any(|arg| arg == "--lock") {
        if let Some(user) = state.users.get_mut(&name) {
            user.locked = true;
        }
        return Ok(out(0, ""));
    }
    if args.iter().any(|arg| arg == "--unlock") {
        if let Some(user) = state.users.get_mut(&name) {
            user.locked = false;
        }
        return Ok(out(0, ""));
    }
    if let Some(groups) = flag_value(args, "--groups") {
        for group in groups.split(',') {
            if let Some(user) = state.users.get_mut(&name) {
                user.groups.insert(group.to_owned());
            }
            state
                .groups
                .entry(group.to_owned())
                .or_insert(GroupState {
                    gid: state.next_gid,
                    members: BTreeSet::new(),
                })
                .members
                .insert(name.clone());
        }
    }
    Ok(out(0, ""))
}

/// `userdel`.
pub(super) fn userdel(state: &mut State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let Some(name) = trailing_name(args) else {
        return Ok(err(2, "userdel: no user name\n"));
    };
    state.users.remove(&name);
    for group in state.groups.values_mut() {
        group.members.remove(&name);
    }
    Ok(out(0, ""))
}

/// `gpasswd --delete <user> <group>`.
pub(super) fn gpasswd(state: &mut State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let tokens: Vec<&str> = args.iter().map(String::as_str).collect();
    if let ["--delete", user, group] = tokens.as_slice() {
        if let Some(state_group) = state.groups.get_mut(*group) {
            state_group.members.remove(*user);
        }
        if let Some(account) = state.users.get_mut(*user) {
            account.groups.remove(*group);
        }
        return Ok(out(0, ""));
    }
    Ok(err(1, "gpasswd: the fake does not model this invocation\n"))
}

/// `id -nG <name>`.
pub(super) fn id(state: &State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let Some(name) = trailing_name(args) else {
        return Ok(err(1, "id: no user name\n"));
    };
    let Some(user) = state.users.get(&name) else {
        return Ok(err(1, format!("id: '{name}': no such user\n")));
    };
    let primary = state
        .groups
        .iter()
        .find(|(_, group)| group.gid == user.gid)
        .map(|(name, _)| name.clone());
    let mut groups: Vec<String> = user.groups.iter().cloned().collect();
    if let Some(primary) = primary {
        groups.insert(0, primary);
    }
    if !groups.contains(&name) {
        groups.insert(0, name.clone());
    }
    Ok(out(0, groups.join(" ")))
}

/// `firewall-cmd` (firewalld).
pub(super) fn firewall_cmd(
    state: &mut State,
    args: &[String],
) -> Result<CommandOutput, LinuxError> {
    let tokens: Vec<&str> = args.iter().map(String::as_str).collect();
    match tokens.as_slice() {
        ["--list-all"] => Ok(out(0, firewalld_listing(state))),
        ["--permanent", "--add-port", port] => {
            state.firewall_ports.insert((*port).to_owned());
            Ok(out(0, "success"))
        }
        ["--permanent", "--remove-port", port] => {
            state.firewall_ports.remove(*port);
            Ok(out(0, "success"))
        }
        ["--permanent", "--add-source", source] => {
            state.firewall_sources.insert((*source).to_owned());
            Ok(out(0, "success"))
        }
        ["--permanent", "--remove-source", source] => {
            state.firewall_sources.remove(*source);
            Ok(out(0, "success"))
        }
        ["--reload"] => Ok(out(0, "success")),
        _ => Ok(err(
            1,
            "firewall-cmd: the fake does not model this invocation\n",
        )),
    }
}

/// The `firewall-cmd --list-all` text.
fn firewalld_listing(state: &State) -> String {
    let ports: Vec<&str> = state.firewall_ports.iter().map(String::as_str).collect();
    let sources: Vec<&str> = state.firewall_sources.iter().map(String::as_str).collect();
    format!(
        "public (active)\n  target: default\n  interfaces: eth0\n  sources: {}\n  services: ssh dhcpv6-client\n  ports: {}\n",
        sources.join(" "),
        ports.join(" ")
    )
}

/// `ufw`.
pub(super) fn ufw(state: &mut State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let tokens: Vec<&str> = args.iter().map(String::as_str).collect();
    match tokens.as_slice() {
        ["status", "numbered"] => Ok(out(0, ufw_listing(state))),
        ["status"] => Ok(out(0, ufw_listing(state))),
        ["allow", spec @ ..] => {
            apply_ufw_spec(state, spec, true);
            Ok(out(0, "Rule added"))
        }
        ["delete", "allow", spec @ ..] => {
            apply_ufw_spec(state, spec, false);
            Ok(out(0, "Rule deleted"))
        }
        _ => Ok(err(1, "ufw: the fake does not model this invocation\n")),
    }
}

/// Applies a ufw rule specification (port and/or source) to the state.
fn apply_ufw_spec(state: &mut State, spec: &[&str], add: bool) {
    let mut port: Option<String> = None;
    let mut source: Option<String> = None;
    let mut index = 0;
    while index < spec.len() {
        match spec[index] {
            "from" => {
                source = spec.get(index + 1).map(|value| (*value).to_owned());
                index += 2;
            }
            "port" => {
                let value = spec.get(index + 1).copied().unwrap_or_default();
                let protocol = spec
                    .get(index + 3)
                    .copied()
                    .filter(|token| *token == "tcp" || *token == "udp")
                    .unwrap_or("tcp");
                port = Some(format!("{value}/{protocol}"));
                index += 4;
            }
            token => {
                if token.contains('/') {
                    port = Some(token.to_owned());
                }
                index += 1;
            }
        }
    }
    if let Some(port) = port {
        if add {
            state.firewall_ports.insert(port);
        } else {
            state.firewall_ports.remove(&port);
        }
    }
    if let Some(source) = source {
        if add {
            state.firewall_sources.insert(source);
        } else {
            state.firewall_sources.remove(&source);
        }
    }
}

/// The `ufw status numbered` text.
fn ufw_listing(state: &State) -> String {
    let mut out = String::from(
        "Status: active\n\n     To                         Action      From\n--                         ------      ----\n",
    );
    let mut index = 1;
    for port in &state.firewall_ports {
        out.push_str(&format!(
            "[ {index}] {port}                     ALLOW       Anywhere\n"
        ));
        index += 1;
    }
    for source in &state.firewall_sources {
        out.push_str(&format!(
            "[ {index}] Anywhere                   ALLOW       {source}\n"
        ));
        index += 1;
    }
    out
}

/// `nft` (nftables).
pub(super) fn nft(state: &mut State, args: &[String]) -> Result<CommandOutput, LinuxError> {
    let tokens: Vec<&str> = args.iter().map(String::as_str).collect();
    match tokens.as_slice() {
        ["--handle", "list", "ruleset"] => Ok(out(0, nft_ruleset(state))),
        ["--handle", "list", "chain", "inet", "spot", "input"] => Ok(out(0, nft_chain(state))),
        ["add", "rule", "inet", "spot", "input", expression @ ..] => {
            let key = expression.join(" ");
            let handle = state.next_handle;
            state.next_handle += 1;
            state.nft_handles.insert(key, handle);
            Ok(out(0, ""))
        }
        ["delete", "rule", "inet", "spot", "input", "handle", handle] => {
            let handle: u32 = handle.parse().unwrap_or(0);
            state.nft_handles.retain(|_, value| *value != handle);
            Ok(out(0, ""))
        }
        _ => Ok(err(1, "nft: the fake does not model this invocation\n")),
    }
}

/// The nftables ruleset listing.
fn nft_ruleset(state: &State) -> String {
    format!(
        "table inet spot {{\n\tchain input {{\n{}\t}}\n}}\n",
        nft_chain(state)
    )
}

/// The nftables chain listing, one rule per line with its handle.
fn nft_chain(state: &State) -> String {
    let mut out = String::new();
    for (expression, handle) in &state.nft_handles {
        out.push_str(&format!("\t\t{expression} # handle {handle}\n"));
    }
    out
}
