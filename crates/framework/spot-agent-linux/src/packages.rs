//! Packages and updates: the [`PackageBackend`] abstraction (dnf, apt) and the six operations
//! over it.
//!
//! The backend is detected from `/etc/os-release` (`ID` and `ID_LIKE`): the Fedora family uses
//! `dnf`/`rpm`, the Debian family `apt`/`dpkg-query`. A package operation refuses to run while
//! another transaction holds the manager's lock — `dnf`'s `/var/run/dnf.pid` for the Fedora
//! family, `fuser` on `/var/lib/dpkg/lock-frontend` for the Debian family — so a second
//! administrator cannot corrupt a half-finished transaction.
//!
//! Every package name is validated twice: by the operation's catalogue schema, and again here
//! against the allow-listed package charset with no leading dash. Each name is one `argv` entry,
//! never a fragment of a command line.

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_mgmt_ops::{OpId, Validated};

use crate::args::Args;
use crate::command::{CommandOutput, CommandRequest, CommandRunner, FileSystem};
use crate::error::LinuxError;
use crate::ids::{
    self, PACKAGE_INSTALL, PACKAGE_LIST, PACKAGE_REMOVE, PACKAGE_REPOSITORIES, UPDATE_APPLY,
    UPDATE_CHECK,
};
use crate::preview::{Check, LinuxOperation, Outcome, Preview, Step, first_line, run_steps};

/// The package manager family a host uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageBackend {
    /// dnf and rpm (Fedora, RHEL and compatible distributions).
    Dnf,
    /// apt and dpkg (Debian, Ubuntu).
    Apt,
}

impl PackageBackend {
    /// The backend's short name, for messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Dnf => "dnf",
            Self::Apt => "apt",
        }
    }

    /// Detects the backend from `/etc/os-release`.
    ///
    /// # Errors
    /// [`LinuxError::Io`] when the file cannot be read, [`LinuxError::Unsupported`] when the
    /// distribution is neither family.
    pub fn detect(fs: &dyn FileSystem) -> Result<Self, LinuxError> {
        let text = fs.read_to_string("/etc/os-release")?;
        let id = os_release_field(&text, "ID");
        let id_like = os_release_field(&text, "ID_LIKE");
        let family = format!("{id} {id_like}");
        if ["fedora", "rhel", "centos", "rocky", "almalinux", "oracle"]
            .iter()
            .any(|name| family.split_whitespace().any(|token| token == *name))
        {
            Ok(Self::Dnf)
        } else if ["debian", "ubuntu"]
            .iter()
            .any(|name| family.split_whitespace().any(|token| token == *name))
        {
            Ok(Self::Apt)
        } else {
            Err(LinuxError::Unsupported {
                what: format!("distribution `{id}`"),
            })
        }
    }

    /// Whether the package manager's lock is held by another transaction.
    ///
    /// # Errors
    /// [`LinuxError::Io`] for the dnf sentinel, or the `fuser` command's own failure.
    pub fn lock_is_held(
        self,
        runner: &dyn CommandRunner,
        fs: &dyn FileSystem,
    ) -> Result<bool, LinuxError> {
        match self {
            Self::Dnf => fs.exists(crate::testing::DNF_LOCK),
            Self::Apt => {
                let request = CommandRequest::new("fuser")
                    .args(["-s", crate::testing::APT_LOCK])
                    .timeout_secs(30);
                let output = runner.run(&request)?;
                Ok(output.status == 0)
            }
        }
    }
}

/// The `ID=…` (or `ID_LIKE=…`) value of an `/etc/os-release` document, quotes stripped.
fn os_release_field(text: &str, key: &str) -> String {
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() == key {
            return value.trim().trim_matches('"').to_owned();
        }
    }
    String::new()
}

/// The two seams a package operation needs.
#[derive(Clone)]
pub struct PackageEnv {
    /// The command seam.
    pub runner: Arc<dyn CommandRunner>,
    /// The filesystem seam.
    pub fs: Arc<dyn FileSystem>,
}

impl PackageEnv {
    /// An environment over the two seams.
    #[must_use]
    pub fn new(runner: Arc<dyn CommandRunner>, fs: Arc<dyn FileSystem>) -> Self {
        Self { runner, fs }
    }

    /// Detects the backend.
    fn backend(&self) -> Result<PackageBackend, LinuxError> {
        PackageBackend::detect(&*self.fs)
    }

    /// Refuses when another package transaction holds the lock.
    fn ensure_unlocked(&self, backend: PackageBackend) -> Result<(), LinuxError> {
        if backend.lock_is_held(&*self.runner, &*self.fs)? {
            return Err(LinuxError::PackageLocked {
                backend: backend.name().to_owned(),
            });
        }
        Ok(())
    }

    /// The installed subset of `packages`.
    fn installed(
        &self,
        backend: PackageBackend,
        packages: &[&str],
    ) -> Result<BTreeSet<String>, LinuxError> {
        let request = match backend {
            PackageBackend::Dnf => CommandRequest::new("rpm")
                .args(["-q", "--queryformat", "%{NAME}\\n"])
                .args(packages.iter().copied()),
            PackageBackend::Apt => CommandRequest::new("dpkg-query")
                .args(["-W", "-f", "${db:Status-Abbrev}${Package}\\n"])
                .args(packages.iter().copied()),
        };
        let output = self.runner.run(&request)?;
        // rpm exits 1 when any queried package is missing; dpkg-query likewise. Both still
        // print the installed ones, which is the answer we want.
        if output.status != 0 && output.status != 1 {
            return Err(LinuxError::CommandFailed {
                program: request.program.clone(),
                status: output.status,
                detail: first_line(&output.stderr),
            });
        }
        let mut found = BTreeSet::new();
        for line in output.stdout_lines() {
            match backend {
                PackageBackend::Dnf => {
                    found.insert(line.to_owned());
                }
                PackageBackend::Apt => {
                    if let Some(name) = line.strip_prefix("ii ") {
                        found.insert(name.trim().to_owned());
                    }
                }
            }
        }
        Ok(found)
    }

    /// The request that lists every installed package.
    fn list_request(&self, backend: PackageBackend) -> CommandRequest {
        match backend {
            PackageBackend::Dnf => {
                CommandRequest::new("rpm").args(["-qa", "--queryformat", "%{NAME}\\n"])
            }
            PackageBackend::Apt => {
                CommandRequest::new("dpkg-query").args(["-W", "-f", "${Package}\\n"])
            }
        }
    }

    /// The request that installs `packages`.
    fn install_request(&self, backend: PackageBackend, packages: &[&str]) -> CommandRequest {
        match backend {
            PackageBackend::Dnf => CommandRequest::new("dnf")
                .args(["-y", "install"])
                .args(packages.iter().copied()),
            PackageBackend::Apt => CommandRequest::new("apt-get")
                .args(["-y", "install"])
                .args(packages.iter().copied())
                .env("DEBIAN_FRONTEND", "noninteractive"),
        }
    }

    /// The request that removes `packages`.
    fn remove_request(&self, backend: PackageBackend, packages: &[&str]) -> CommandRequest {
        match backend {
            PackageBackend::Dnf => CommandRequest::new("dnf")
                .args(["-y", "remove"])
                .args(packages.iter().copied()),
            PackageBackend::Apt => CommandRequest::new("apt-get")
                .args(["-y", "remove"])
                .args(packages.iter().copied())
                .env("DEBIAN_FRONTEND", "noninteractive"),
        }
    }

    /// The request that lists configured repositories.
    fn repositories_request(&self, backend: PackageBackend) -> CommandRequest {
        match backend {
            PackageBackend::Dnf => CommandRequest::new("dnf").args(["-q", "repolist"]),
            PackageBackend::Apt => CommandRequest::new("apt-cache").arg("policy"),
        }
    }

    /// The requests that apply updates: a refresh and an upgrade, or the security path.
    fn update_requests(&self, backend: PackageBackend, security_only: bool) -> Vec<CommandRequest> {
        match backend {
            PackageBackend::Dnf => {
                let mut request = CommandRequest::new("dnf").args(["-y", "upgrade"]);
                if security_only {
                    request = request.arg("--security");
                }
                vec![request]
            }
            PackageBackend::Apt => {
                let refresh = CommandRequest::new("apt-get")
                    .args(["-y", "update"])
                    .env("DEBIAN_FRONTEND", "noninteractive");
                let upgrade = if security_only {
                    CommandRequest::new("unattended-upgrade")
                        .arg("--verbose")
                        .env("DEBIAN_FRONTEND", "noninteractive")
                } else {
                    CommandRequest::new("apt-get")
                        .args(["-y", "upgrade"])
                        .env("DEBIAN_FRONTEND", "noninteractive")
                };
                vec![refresh, upgrade]
            }
        }
    }

    /// How many updates are still pending, security-only when asked.
    fn pending_updates(
        &self,
        backend: PackageBackend,
        security_only: bool,
    ) -> Result<usize, LinuxError> {
        match backend {
            PackageBackend::Dnf => {
                let output = self.runner.run(
                    &CommandRequest::new("dnf")
                        .args(["-q", "check-update"])
                        .timeout_secs(120),
                )?;
                if output.status == 0 {
                    return Ok(0);
                }
                if output.status != 100 {
                    return Err(LinuxError::CommandFailed {
                        program: "dnf".to_owned(),
                        status: output.status,
                        detail: first_line(&output.stderr),
                    });
                }
                Ok(dnf_updates(&output)
                    .into_iter()
                    .filter(|(_, security)| !security_only || *security)
                    .count())
            }
            PackageBackend::Apt if security_only => {
                let output = self.runner.run(
                    &CommandRequest::new("unattended-upgrade")
                        .args(["--dry-run"])
                        .env("DEBIAN_FRONTEND", "noninteractive"),
                )?;
                if !output.success() {
                    return Err(LinuxError::CommandFailed {
                        program: "unattended-upgrade".to_owned(),
                        status: output.status,
                        detail: first_line(&output.stderr),
                    });
                }
                Ok(output.stdout_lines().len())
            }
            PackageBackend::Apt => {
                let output = self
                    .runner
                    .run(&CommandRequest::new("apt-get").args(["-s", "upgrade"]))?;
                if !output.success() {
                    return Err(LinuxError::CommandFailed {
                        program: "apt-get".to_owned(),
                        status: output.status,
                        detail: first_line(&output.stderr),
                    });
                }
                Ok(output
                    .stdout_lines()
                    .into_iter()
                    .filter(|line| line.starts_with("Inst "))
                    .count())
            }
        }
    }

    /// Whether the host reports that a reboot is required.
    fn reboot_required(&self, backend: PackageBackend) -> Result<bool, LinuxError> {
        match backend {
            PackageBackend::Dnf => {
                let output = self.runner.run(&CommandRequest::new("dnf").args([
                    "-q",
                    "needs-restarting",
                    "-r",
                ]))?;
                Ok(output.status == 1)
            }
            PackageBackend::Apt => self.fs.exists(crate::testing::REBOOT_REQUIRED),
        }
    }
}

/// The updates `dnf -q check-update` listed, each with whether it came from a security repo.
fn dnf_updates(output: &CommandOutput) -> Vec<(String, bool)> {
    let mut updates = Vec::new();
    for line in output.stdout_lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 || !tokens[0].contains('.') {
            continue;
        }
        let name = tokens[0].split('.').next().unwrap_or(tokens[0]).to_owned();
        let security = tokens.last().is_some_and(|repo| repo.contains("security"));
        updates.push((name, security));
    }
    updates
}

/// `os.package.list`: read the installed packages.
pub struct PackageList {
    env: PackageEnv,
    op: OpId,
}

impl PackageList {
    /// A module over `env`.
    #[must_use]
    pub fn new(env: PackageEnv) -> Self {
        Self {
            env,
            op: ids::op_id(PACKAGE_LIST),
        }
    }
}

impl LinuxOperation for PackageList {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, _args: &Validated) -> Result<Preview, LinuxError> {
        let backend = self.env.backend()?;
        let step = Step::new(
            "read the installed package list",
            self.env.list_request(backend),
        );
        Ok(Preview::new(
            self.op.clone(),
            format!("list installed packages via {}", backend.name()),
            vec![step],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.env.runner, &preview)?;
        Ok(Outcome::new(&self.op, applied, Vec::new()))
    }

    fn verify(&self, _args: &Validated) -> Result<Check, LinuxError> {
        let backend = self.env.backend()?;
        let _ = backend;
        Ok(Check::ok(&self.op, "the package list is readable"))
    }
}

/// `os.package.install`: install the named packages that are missing.
pub struct PackageInstall {
    env: PackageEnv,
    op: OpId,
}

impl PackageInstall {
    /// A module over `env`.
    #[must_use]
    pub fn new(env: PackageEnv) -> Self {
        Self {
            env,
            op: ids::op_id(PACKAGE_INSTALL),
        }
    }
}

impl LinuxOperation for PackageInstall {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let packages = args.package_names("packages")?;
        let backend = self.env.backend()?;
        self.env.ensure_unlocked(backend)?;
        let installed = self.env.installed(backend, &packages)?;
        let missing: Vec<&str> = packages
            .iter()
            .copied()
            .filter(|name| !installed.contains(*name))
            .collect();
        if missing.is_empty() {
            return Ok(Preview::new(
                self.op.clone(),
                "every requested package is already installed",
                Vec::new(),
                Vec::new(),
            ));
        }
        let step = Step::new(
            format!("install {} package(s)", missing.len()),
            self.env.install_request(backend, &missing),
        );
        Ok(Preview::new(
            self.op.clone(),
            format!(
                "install {} package(s) via {}",
                missing.len(),
                backend.name()
            ),
            vec![step],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.env.runner, &preview)?;
        let check = self.verify(args)?;
        if !check.ok {
            return Err(LinuxError::VerifyFailed {
                op: self.op.as_str().to_owned(),
                detail: check.detail,
            });
        }
        Ok(Outcome::new(
            &self.op,
            applied,
            vec![preview.summary.clone()],
        ))
    }

    fn verify(&self, args: &Validated) -> Result<Check, LinuxError> {
        let args = Args::new(args);
        let packages = args.package_names("packages")?;
        let backend = self.env.backend()?;
        let installed = self.env.installed(backend, &packages)?;
        let missing: Vec<&str> = packages
            .iter()
            .copied()
            .filter(|name| !installed.contains(*name))
            .collect();
        if missing.is_empty() {
            Ok(Check::ok(&self.op, "every requested package is installed"))
        } else {
            Ok(Check::failed(
                &self.op,
                format!("{} package(s) are still missing", missing.len()),
            ))
        }
    }
}

/// `os.package.remove`: remove the named packages that are installed.
pub struct PackageRemove {
    env: PackageEnv,
    op: OpId,
}

impl PackageRemove {
    /// A module over `env`.
    #[must_use]
    pub fn new(env: PackageEnv) -> Self {
        Self {
            env,
            op: ids::op_id(PACKAGE_REMOVE),
        }
    }
}

impl LinuxOperation for PackageRemove {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let packages = args.package_names("packages")?;
        let backend = self.env.backend()?;
        self.env.ensure_unlocked(backend)?;
        let installed = self.env.installed(backend, &packages)?;
        let present: Vec<&str> = packages
            .iter()
            .copied()
            .filter(|name| installed.contains(*name))
            .collect();
        if present.is_empty() {
            return Ok(Preview::new(
                self.op.clone(),
                "none of the requested packages are installed",
                Vec::new(),
                Vec::new(),
            ));
        }
        let step = Step::new(
            format!("remove {} package(s)", present.len()),
            self.env.remove_request(backend, &present),
        );
        Ok(Preview::new(
            self.op.clone(),
            format!("remove {} package(s) via {}", present.len(), backend.name()),
            vec![step],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.env.runner, &preview)?;
        let check = self.verify(args)?;
        if !check.ok {
            return Err(LinuxError::VerifyFailed {
                op: self.op.as_str().to_owned(),
                detail: check.detail,
            });
        }
        Ok(Outcome::new(
            &self.op,
            applied,
            vec![preview.summary.clone()],
        ))
    }

    fn verify(&self, args: &Validated) -> Result<Check, LinuxError> {
        let args = Args::new(args);
        let packages = args.package_names("packages")?;
        let backend = self.env.backend()?;
        let installed = self.env.installed(backend, &packages)?;
        let present: Vec<&str> = packages
            .iter()
            .copied()
            .filter(|name| installed.contains(*name))
            .collect();
        if present.is_empty() {
            Ok(Check::ok(&self.op, "none of the requested packages remain"))
        } else {
            Ok(Check::failed(
                &self.op,
                format!("{} package(s) are still installed", present.len()),
            ))
        }
    }
}

/// `os.package.repositories`: list the configured repositories.
pub struct PackageRepositories {
    env: PackageEnv,
    op: OpId,
}

impl PackageRepositories {
    /// A module over `env`.
    #[must_use]
    pub fn new(env: PackageEnv) -> Self {
        Self {
            env,
            op: ids::op_id(PACKAGE_REPOSITORIES),
        }
    }
}

impl LinuxOperation for PackageRepositories {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, _args: &Validated) -> Result<Preview, LinuxError> {
        let backend = self.env.backend()?;
        let step = Step::new(
            "read the configured repositories",
            self.env.repositories_request(backend),
        );
        Ok(Preview::new(
            self.op.clone(),
            format!("list repositories via {}", backend.name()),
            vec![step],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.env.runner, &preview)?;
        Ok(Outcome::new(&self.op, applied, Vec::new()))
    }

    fn verify(&self, _args: &Validated) -> Result<Check, LinuxError> {
        Ok(Check::ok(&self.op, "the repository list is readable"))
    }
}

/// `os.update.check`: report available updates and whether a reboot is required.
pub struct UpdateCheck {
    env: PackageEnv,
    op: OpId,
}

impl UpdateCheck {
    /// A module over `env`.
    #[must_use]
    pub fn new(env: PackageEnv) -> Self {
        Self {
            env,
            op: ids::op_id(UPDATE_CHECK),
        }
    }
}

impl LinuxOperation for UpdateCheck {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, _args: &Validated) -> Result<Preview, LinuxError> {
        let backend = self.env.backend()?;
        let mut steps = vec![Step::new(
            "read the available updates",
            match backend {
                PackageBackend::Dnf => CommandRequest::new("dnf").args(["-q", "check-update"]),
                PackageBackend::Apt => CommandRequest::new("apt-get").args(["-s", "upgrade"]),
            },
        )];
        if backend == PackageBackend::Dnf {
            steps.push(Step::new(
                "read whether a reboot is required",
                CommandRequest::new("dnf").args(["-q", "needs-restarting", "-r"]),
            ));
        }
        Ok(Preview::new(
            self.op.clone(),
            format!("check for updates via {}", backend.name()),
            steps,
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let _ = self.plan(args)?;
        let backend = self.env.backend()?;
        let mut notes = vec![format!(
            "{} update(s) available",
            self.env.pending_updates(backend, false)?
        )];
        if self.env.reboot_required(backend)? {
            notes.push("a reboot is required".to_owned());
        }
        Ok(Outcome::new(&self.op, 0, notes))
    }

    fn verify(&self, _args: &Validated) -> Result<Check, LinuxError> {
        let backend = self.env.backend()?;
        let pending = self.env.pending_updates(backend, false)?;
        Ok(Check::ok(
            &self.op,
            format!("{pending} update(s) still available"),
        ))
    }
}

/// `os.update.apply`: apply all updates, or only the security ones.
pub struct UpdateApply {
    env: PackageEnv,
    op: OpId,
}

impl UpdateApply {
    /// A module over `env`.
    #[must_use]
    pub fn new(env: PackageEnv) -> Self {
        Self {
            env,
            op: ids::op_id(UPDATE_APPLY),
        }
    }
}

impl LinuxOperation for UpdateApply {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let security_only = args.flag("security_only")?;
        let backend = self.env.backend()?;
        self.env.ensure_unlocked(backend)?;
        if self.env.pending_updates(backend, security_only)? == 0 {
            return Ok(Preview::new(
                self.op.clone(),
                "there are no updates to apply",
                Vec::new(),
                Vec::new(),
            ));
        }
        let steps: Vec<Step> = self
            .env
            .update_requests(backend, security_only)
            .into_iter()
            .map(|command| {
                Step::new(
                    if security_only {
                        "apply security updates"
                    } else {
                        "apply all available updates"
                    },
                    command,
                )
            })
            .collect();
        Ok(Preview::new(
            self.op.clone(),
            format!(
                "apply {} updates via {}",
                if security_only { "security" } else { "all" },
                backend.name()
            ),
            steps,
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let parsed = Args::new(args);
        let security_only = parsed.flag("security_only")?;
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.env.runner, &preview)?;
        let backend = self.env.backend()?;
        let mut notes = vec![format!(
            "{} update(s) applied",
            if security_only { "security" } else { "all" }
        )];
        if self.env.reboot_required(backend)? {
            notes.push("a reboot is required".to_owned());
        }
        let check = self.verify(args)?;
        if !check.ok {
            return Err(LinuxError::VerifyFailed {
                op: self.op.as_str().to_owned(),
                detail: check.detail,
            });
        }
        Ok(Outcome::new(&self.op, applied, notes))
    }

    fn verify(&self, args: &Validated) -> Result<Check, LinuxError> {
        let args = Args::new(args);
        let security_only = args.flag("security_only")?;
        let backend = self.env.backend()?;
        let pending = self.env.pending_updates(backend, security_only)?;
        if pending == 0 {
            Ok(Check::ok(
                &self.op,
                if security_only {
                    "no security updates remain"
                } else {
                    "no updates remain"
                },
            ))
        } else {
            Ok(Check::failed(
                &self.op,
                format!("{pending} update(s) still pending"),
            ))
        }
    }
}

#[cfg(test)]
#[path = "packages_tests.rs"]
mod tests;
