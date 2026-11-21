//! `spot-agent service ...` on Linux: the hardened systemd unit, rendered from the template
//! `packaging/spot-agent.service`, and the plan that installs, removes and drives it.
//!
//! Everything is a **plan** first: [`plan_install`] returns the steps (create the account,
//! create the state directory, write the unit, reload systemd, enable the unit) as data, so
//! `--print` shows exactly what would happen and the tests parse the rendered unit and run
//! the plan against a fake [`Host`]. The real [`SystemHost`] writes files and runs
//! `systemctl`, `useradd` and `install` argv-only (never through a shell).
//!
//! # Hardening and its one consequence
//!
//! The unit sets `NoNewPrivileges=yes`, an empty capability set, `ProtectSystem=strict`,
//! a private `/tmp`, only the state directory and the install root writable, and a
//! dedicated unprivileged user. `NoNewPrivileges` also stops `sudo`, so the privileged
//! helper (`sudo -n spot-agent-priv`, see [`crate::privilege`]) cannot raise the agent's
//! privileges: privileged operations are refused until the operator installs with
//! `--privileged-helper`, which renders `NoNewPrivileges=no` and `RestrictSUIDSGID=no` and
//! nothing else weaker. The default is the strict unit.

use std::path::PathBuf;

use crate::privilege::templates::valid_account_name;

/// The template, compiled in so the binary needs no file next to it.
pub const UNIT_TEMPLATE: &str = include_str!("../packaging/spot-agent.service");
/// The unit's file name.
pub const UNIT_NAME: &str = "spot-agent.service";
/// Where the unit is installed.
pub const UNIT_PATH: &str = "/etc/systemd/system/spot-agent.service";
/// The default agent account.
pub const DEFAULT_USER: &str = "spot-agent";
/// Exit code: the binary path is not safe (the same code as the Windows service commands).
pub const EXIT_UNSAFE_PATH: i32 = 10;

/// Directories an ordinary user can write to: a binary there is refused.
const USER_WRITABLE: &[&str] = &["/tmp/", "/var/tmp/", "/dev/shm/", "/home/", "/run/user/"];

/// What the unit is rendered from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitSettings {
    /// The absolute path of the agent executable.
    pub binary: String,
    /// The agent account (and group of the same name).
    pub user: String,
    /// The state directory (`--state-dir` of the service).
    pub state_dir: String,
    /// The install root the self-update writes to.
    pub install_root: String,
    /// Install so `sudo` works for the privileged helper (see the module docs).
    pub privileged_helper: bool,
}

/// Why a setting was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitRefused {
    /// The binary path is not a safe absolute path.
    Binary,
    /// The account name is not a valid POSIX name (or is `root`).
    User,
    /// The state directory or install root is not a plain absolute path.
    Directory,
}

/// A path made of plain characters only: the unit takes it unquoted, so nothing that
/// systemd would interpret (`%`, `$`, spaces, quotes, backslashes) gets through.
fn plain_absolute(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 200
        && !path.contains("//")
        && path.split('/').all(|part| part != "..")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'))
}

/// Checks `settings` and returns the first thing wrong.
///
/// # Errors
/// [`UnitRefused`].
pub fn check_settings(settings: &UnitSettings) -> Result<(), UnitRefused> {
    if !plain_absolute(&settings.binary)
        || USER_WRITABLE
            .iter()
            .any(|dir| settings.binary.starts_with(dir))
    {
        return Err(UnitRefused::Binary);
    }
    if !valid_account_name(&settings.user) || settings.user == "root" {
        return Err(UnitRefused::User);
    }
    if !plain_absolute(&settings.state_dir) || !plain_absolute(&settings.install_root) {
        return Err(UnitRefused::Directory);
    }
    Ok(())
}

/// Renders the unit text.
///
/// # Errors
/// [`UnitRefused`] when a setting is not safe to put in a unit.
pub fn render_unit(settings: &UnitSettings) -> Result<String, UnitRefused> {
    check_settings(settings)?;
    let (no_new, suid, note) = if settings.privileged_helper {
        (
            "no",
            "no",
            "relaxed on purpose: installed with --privileged-helper so sudo can run the helper",
        )
    } else {
        (
            "yes",
            "yes",
            "so the sudo-based privileged helper is unavailable: install with --privileged-helper to allow it",
        )
    };
    Ok(UNIT_TEMPLATE
        .replace("@PRIVILEGE_NOTE@", note)
        .replace("@NO_NEW_PRIVILEGES@", no_new)
        .replace("@RESTRICT_SUID@", suid)
        .replace("@USER@", &settings.user)
        .replace("@GROUP@", &settings.user)
        .replace("@BINARY@", &settings.binary)
        .replace("@STATE_DIR@", &settings.state_dir)
        .replace("@INSTALL_ROOT@", &settings.install_root))
}

/// One step of an installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Run `argv[0]` with `argv[1..]`; no shell.
    Run(Vec<String>),
    /// Write `content` to `path` with `mode`, atomically replacing a file.
    Write {
        /// The file.
        path: PathBuf,
        /// Its text.
        content: String,
        /// Its mode (0644 for the unit).
        mode: u32,
    },
    /// Remove a file; missing is fine.
    Remove(PathBuf),
}

fn run(parts: &[&str]) -> Step {
    Step::Run(parts.iter().map(|p| (*p).to_owned()).collect())
}

/// What the host already has, so a second install is a no-op where it can be.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Existing {
    /// The installed unit text, when there is one.
    pub unit: Option<String>,
    /// Whether the account exists.
    pub user_exists: bool,
}

/// The steps that bring the host to `settings`.
///
/// # Errors
/// [`UnitRefused`] when a setting is not safe.
pub fn plan_install(
    settings: &UnitSettings,
    existing: &Existing,
) -> Result<Vec<Step>, UnitRefused> {
    let unit = render_unit(settings)?;
    let mut steps = Vec::new();
    if !existing.user_exists {
        steps.push(run(&[
            "useradd",
            "--system",
            "--user-group",
            "--no-create-home",
            "--home-dir",
            &settings.state_dir,
            "--shell",
            "/usr/sbin/nologin",
            &settings.user,
        ]));
    }
    steps.push(run(&[
        "install",
        "-d",
        "-m",
        "0700",
        "-o",
        &settings.user,
        "-g",
        &settings.user,
        &settings.state_dir,
    ]));
    if existing.unit.as_deref() != Some(unit.as_str()) {
        steps.push(Step::Write {
            path: PathBuf::from(UNIT_PATH),
            content: unit,
            mode: 0o644,
        });
    }
    steps.push(run(&["systemctl", "daemon-reload"]));
    steps.push(run(&["systemctl", "enable", UNIT_NAME]));
    Ok(steps)
}

/// The steps that remove the service: disable and stop it, delete the unit, reload. The
/// account and the state directory stay (they hold the agent's identity and audit log).
#[must_use]
pub fn plan_uninstall(existing: &Existing) -> Vec<Step> {
    if existing.unit.is_none() {
        return Vec::new();
    }
    vec![
        run(&["systemctl", "disable", "--now", UNIT_NAME]),
        Step::Remove(PathBuf::from(UNIT_PATH)),
        run(&["systemctl", "daemon-reload"]),
    ]
}

/// The step for `start` or `stop`.
#[must_use]
pub fn plan_control(verb: &str) -> Vec<Step> {
    vec![run(&["systemctl", verb, UNIT_NAME])]
}

/// The step for `status`.
#[must_use]
pub fn plan_status() -> Vec<Step> {
    vec![run(&["systemctl", "status", "--no-pager", UNIT_NAME])]
}

/// One step as the text `--print` shows. A file shows its path, mode and full content.
#[must_use]
pub fn render_step(step: &Step) -> String {
    match step {
        Step::Run(argv) => argv.join(" "),
        Step::Write {
            path,
            content,
            mode,
        } => format!(
            "write {} (mode {mode:04o}):\n{}",
            path.display(),
            content.trim_end()
        ),
        Step::Remove(path) => format!("remove {}", path.display()),
    }
}

/// The seam through which a plan touches the machine.
pub trait Host {
    /// Runs `argv` and returns its exit code.
    ///
    /// # Errors
    /// An I/O error when the program cannot be run.
    fn run(&self, argv: &[String]) -> std::io::Result<i32>;
    /// Writes a file atomically.
    ///
    /// # Errors
    /// An I/O error.
    fn write(&self, path: &std::path::Path, content: &str, mode: u32) -> std::io::Result<()>;
    /// Removes a file; missing is not an error.
    ///
    /// # Errors
    /// An I/O error other than "not found".
    fn remove(&self, path: &std::path::Path) -> std::io::Result<()>;
    /// The installed unit text, if any.
    fn read_unit(&self) -> Option<String>;
    /// Whether the account exists.
    fn user_exists(&self, user: &str) -> bool;
}

/// What the host has now.
pub fn existing(host: &dyn Host, user: &str) -> Existing {
    Existing {
        unit: host.read_unit(),
        user_exists: host.user_exists(user),
    }
}

/// Runs the steps in order and stops at the first failure. The result is a process exit
/// code: 0, or the failing command's code (1 when it could not run).
pub fn execute(host: &dyn Host, steps: &[Step]) -> i32 {
    for step in steps {
        let outcome = match step {
            Step::Run(argv) => match host.run(argv) {
                Ok(0) => Ok(()),
                Ok(code) => return code.max(1),
                Err(error) => Err(error),
            },
            Step::Write {
                path,
                content,
                mode,
            } => host.write(path, content, *mode),
            Step::Remove(path) => host.remove(path),
        };
        if outcome.is_err() {
            return 1;
        }
    }
    0
}

/// The real host: files and argv-only commands.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemHost;

impl Host for SystemHost {
    fn run(&self, argv: &[String]) -> std::io::Result<i32> {
        let Some((program, args)) = argv.split_first() else {
            return Ok(1);
        };
        let status = std::process::Command::new(program).args(args).status()?;
        Ok(status.code().unwrap_or(1))
    }

    fn write(&self, path: &std::path::Path, content: &str, mode: u32) -> std::io::Result<()> {
        use std::io::Write as _;
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".new");
        let temporary = PathBuf::from(temporary);
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(mode);
            }
            #[cfg(not(unix))]
            let _ = mode;
            let mut file = options.open(&temporary)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, path)
    }

    fn remove(&self, path: &std::path::Path) -> std::io::Result<()> {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    fn read_unit(&self) -> Option<String> {
        std::fs::read_to_string(UNIT_PATH).ok()
    }

    fn user_exists(&self, user: &str) -> bool {
        std::process::Command::new("id")
            .args(["-u", user])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
