//! The state directory and its files: layout, permission checks, atomic writes.
//!
//! Linux: `/var/lib/spot-agent`, mode 0700. Windows: `%ProgramData%\Spotledger\Agent`
//! with an ACL restricted to SYSTEM and Administrators.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::AgentError;

/// SDDL of the Windows state directory: protected DACL, full control for SYSTEM (SY) and
/// built-in Administrators (BA), inherited by children, nobody else.
pub const WINDOWS_STATE_SDDL: &str = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// The `icacls` arguments that apply [`WINDOWS_STATE_SDDL`]'s effect to `path`.
#[must_use]
pub fn windows_icacls_args(path: &str) -> Vec<String> {
    vec![
        path.to_owned(),
        "/inheritance:r".to_owned(),
        "/grant:r".to_owned(),
        "*S-1-5-18:(OI)(CI)F".to_owned(),
        "*S-1-5-32-544:(OI)(CI)F".to_owned(),
    ]
}

/// The default state directory of this platform.
#[must_use]
pub fn default_state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
        base.join("Spotledger").join("Agent")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/var/lib/spot-agent")
    }
}

/// The agent's state directory and the files in it.
#[derive(Clone, Debug)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    /// A handle on `root` (nothing is touched yet).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `agent.toml`.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.root.join("agent.toml")
    }

    /// The identity key file (the enrolment identity is written next to it as `.identity`).
    #[must_use]
    pub fn key_path(&self) -> PathBuf {
        self.root.join("agent.key")
    }

    /// The trusted administrator keys.
    #[must_use]
    pub fn trust_path(&self) -> PathBuf {
        self.root.join("trust.toml")
    }

    /// The local command audit log.
    #[must_use]
    pub fn audit_path(&self) -> PathBuf {
        self.root.join("audit.log")
    }

    /// The structured log file.
    #[must_use]
    pub fn log_path(&self) -> PathBuf {
        self.root.join("agent.log")
    }

    /// The persisted replay and command state (T3).
    #[must_use]
    pub fn commands_dir(&self) -> PathBuf {
        self.root.join("commands")
    }

    /// Creates the directory when missing (mode 0700 / restricted ACL).
    ///
    /// # Errors
    /// [`AgentError::StateIo`] when it cannot be created.
    pub fn create(&self) -> Result<(), AgentError> {
        if self.root.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|_| AgentError::StateIo("cannot create the state directory".to_owned()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700)).map_err(
                |_| AgentError::StateIo("cannot restrict the state directory".to_owned()),
            )?;
        }
        #[cfg(windows)]
        {
            let args = windows_icacls_args(&self.root.display().to_string());
            let status = std::process::Command::new("icacls")
                .args(&args)
                .stdout(std::process::Stdio::null())
                .status()
                .map_err(|_| AgentError::StateIo("icacls failed to start".to_owned()))?;
            if !status.success() {
                return Err(AgentError::StateIo("icacls refused the ACL".to_owned()));
            }
        }
        Ok(())
    }

    /// Refuses a state directory or file readable beyond its owner (Unix), and a key file
    /// with mode 0644 in particular.
    ///
    /// # Errors
    /// [`AgentError::StatePermissions`] when a mode allows group or other access.
    pub fn check_permissions(&self) -> Result<(), AgentError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode_of = |p: &Path| std::fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
            let dir = mode_of(&self.root)
                .map_err(|_| AgentError::StateIo("cannot stat the state directory".to_owned()))?;
            if dir & 0o077 != 0 {
                return Err(AgentError::StatePermissions(
                    "the state directory must be mode 0700".to_owned(),
                ));
            }
            for path in [self.key_path(), self.trust_path(), self.config_path()] {
                if let Ok(mode) = mode_of(&path)
                    && mode & 0o077 != 0
                {
                    return Err(AgentError::StatePermissions(format!(
                        "{} must not be accessible by group or others",
                        path.file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                    )));
                }
            }
        }
        Ok(())
    }

    /// Whether an enrolment identity exists.
    #[must_use]
    pub fn is_enrolled(&self) -> bool {
        let mut p = self.key_path().into_os_string();
        p.push(".identity");
        Path::new(&p).exists()
    }
}

/// Writes `contents` to `path` atomically (temp file, fsync, rename, fsync of the
/// directory), mode 0600 on Unix.
///
/// # Errors
/// [`AgentError::StateIo`] on any I/O failure.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), AgentError> {
    let io = |_: std::io::Error| AgentError::StateIo("atomic write failed".to_owned());
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(io)?;
    file.write_all(contents).map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(io)?;
    #[cfg(unix)]
    if let Some(dir) = path.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}
