//! The persisted network revert guard (WP-ADM-04.T5).
//!
//! A connectivity-affecting network change is reverted unless it is confirmed in time. The
//! pending ticket must therefore survive the agent: the change itself may restart networking
//! and the agent with it, and the privileged handler builds a fresh [`crate::NetworkModule`]
//! for every operation, so memory cannot carry a ticket from `set` to `confirm`.
//!
//! Two files under the agent state directory hold the guard, each written atomically (temp
//! file, fsync, rename) **before** the change is applied:
//!
//! - the backup (`network-revert.backup.json`): the interface, the backend, the previous
//!   configuration, and the files the change writes. This is what a revert restores.
//! - the ticket (`network-revert.ticket.json`): the id, the window and the deadline.
//!
//! The ticket is the authority: removing it is what confirms (or completes the revert of) a
//! change. A backup without a ticket is a change that never started and is discarded. A
//! corrupt ticket with a readable backup is reverted from the backup and reported, never
//! ignored.
//!
//! # The deadline survives a clock step
//!
//! The ticket carries the unix deadline **and**, when the machine could be asked, the boot id
//! (`/proc/sys/kernel/random/boot_id`) and the integer uptime in seconds (`/proc/uptime`) at
//! apply time. The rule, applied by every check (confirm, sweep, recover, the re-arm delay):
//!
//! - within the boot that applied the change (the boot id read now equals the stored one) the
//!   window is elapsed uptime against `window_secs`; a wall clock stepped back by an NTP
//!   correction or a wrong RTC neither lengthens nor, stepped forward, shortens it, so the
//!   agent and the (monotonic) systemd timer agree;
//! - across a reboot, or when either value could not be read, the unix deadline decides, as
//!   before. A ticket written before these fields existed has none and so uses the unix
//!   deadline.
//!
//! # One revert at a time, across processes
//!
//! The timer runs the revert in a different process from the agent's confirm. Every operation
//! that reads then changes the guard (`apply`, `confirm`, `sweep`, `revert_ticket`,
//! `recover`) therefore holds an exclusive advisory lock on [`LOCK_FILE`] in the state
//! directory, taken without blocking and retried for at most [`LOCK_WAIT_SECS`] seconds
//! (a bounded number of tries with an injected [`crate::sys::Sleeper`]). A taker that cannot
//! get it answers the retryable [`crate::error::NETWORK_REVERT_BUSY`]. The ticket is read
//! again **after** the lock is held, so the second operation sees the first one's result.
//!
//! The second, independent safety is a [`RevertScheduler`] timer (`systemd-run --on-active`)
//! scheduled at apply time, which runs the revert even if the agent is down.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{NETWORK_REVERT_CORRUPT, NETWORK_REVERT_SCHEDULE, SysError, SysResult};
use crate::network::{Addressing, NetworkBackend};
use crate::sys::{CommandRunner, CommandSpec, FileSystem};

/// The agent state directory used when the host does not choose one.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/spot-agent";

/// The ticket file name.
pub const TICKET_FILE: &str = "network-revert.ticket.json";
/// The backup file name.
pub const BACKUP_FILE: &str = "network-revert.backup.json";
/// The ticket id counter file name.
pub const SEQUENCE_FILE: &str = "network-revert.seq";
/// The cross-process lock file name.
pub const LOCK_FILE: &str = "network-revert.lock";
/// The longest an operation waits for the lock, in seconds.
pub const LOCK_WAIT_SECS: u64 = 5;
/// The pause between lock attempts, in milliseconds.
pub(crate) const LOCK_TRY_MILLIS: u64 = 100;
/// Where the kernel publishes the boot id.
pub const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";
/// Where the kernel publishes the uptime.
pub const UPTIME_PATH: &str = "/proc/uptime";
/// The prefix of the transient timer unit; the ticket id follows.
pub const UNIT_PREFIX: &str = "spot-net-revert-";

const FORMAT_VERSION: u32 = 1;

/// The ticket: when the change must be confirmed by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TicketRecord {
    pub version: u32,
    pub id: u64,
    pub interface: String,
    pub applied: String,
    pub window_secs: u64,
    pub applied_at_unix: i64,
    pub deadline_unix: i64,
    /// The boot that applied the change; absent in a ticket written before T7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// Whole seconds of uptime at apply time; absent in a ticket written before T7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_uptime_secs: Option<u64>,
}

/// The boot id and the uptime at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BootMark {
    pub boot_id: String,
    pub uptime_secs: u64,
}

impl BootMark {
    /// Reads the mark through the file-system port; `None` when either value is missing or
    /// unparseable (the caller then falls back to the unix deadline).
    pub(crate) fn read(fs: &dyn FileSystem) -> Option<Self> {
        let boot_id = fs.read_to_string(Path::new(BOOT_ID_PATH)).ok()?;
        let boot_id = boot_id.trim();
        if boot_id.is_empty()
            || boot_id.len() > 64
            || !boot_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        {
            return None;
        }
        let uptime = fs.read_to_string(Path::new(UPTIME_PATH)).ok()?;
        Some(Self {
            boot_id: boot_id.to_owned(),
            uptime_secs: parse_uptime_secs(&uptime)?,
        })
    }
}

/// The whole seconds of the first field of `/proc/uptime` (`"350735.47 234388.90"`), read as
/// text so no float is involved.
pub(crate) fn parse_uptime_secs(text: &str) -> Option<u64> {
    let first = text.split_whitespace().next()?;
    let whole = first.split('.').next()?;
    if whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    whole.parse().ok()
}

/// An addressing as stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AddressingRecord {
    Dhcp,
    Static {
        address: String,
        prefix: u8,
        gateway: Option<String>,
        dns: Vec<String>,
    },
}

impl From<&Addressing> for AddressingRecord {
    fn from(value: &Addressing) -> Self {
        match value {
            Addressing::Dhcp => Self::Dhcp,
            Addressing::Static {
                address,
                prefix,
                gateway,
                dns,
            } => Self::Static {
                address: address.clone(),
                prefix: *prefix,
                gateway: gateway.clone(),
                dns: dns.clone(),
            },
        }
    }
}

impl From<AddressingRecord> for Addressing {
    fn from(value: AddressingRecord) -> Self {
        match value {
            AddressingRecord::Dhcp => Self::Dhcp,
            AddressingRecord::Static {
                address,
                prefix,
                gateway,
                dns,
            } => Self::Static {
                address,
                prefix,
                gateway,
                dns,
            },
        }
    }
}

/// The backup: what a revert restores.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackupRecord {
    pub version: u32,
    pub id: u64,
    pub interface: String,
    pub backend: String,
    pub previous: AddressingRecord,
    pub previous_file: Option<String>,
    pub applied_files: Vec<String>,
}

impl BackupRecord {
    pub(crate) fn backend(&self) -> Option<NetworkBackend> {
        match self.backend.as_str() {
            "networkmanager" => Some(NetworkBackend::NetworkManager),
            "systemd-networkd" => Some(NetworkBackend::SystemdNetworkd),
            _ => None,
        }
    }
}

/// What the state directory holds.
#[derive(Debug)]
pub(crate) enum Loaded {
    /// No pending change.
    Nothing,
    /// A readable ticket with its backup.
    Valid(TicketRecord, BackupRecord),
    /// The ticket is unreadable but the backup is: revert from the backup and report.
    CorruptTicket(BackupRecord, String),
}

/// Reads and writes the guard files through the [`FileSystem`] seam.
pub(crate) struct RevertStore<'a> {
    fs: &'a dyn FileSystem,
    dir: PathBuf,
}

impl<'a> RevertStore<'a> {
    pub(crate) fn new(fs: &'a dyn FileSystem, dir: &Path) -> Self {
        Self {
            fs,
            dir: dir.to_path_buf(),
        }
    }

    /// The state directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The path of the lock file.
    pub(crate) fn lock_path(&self) -> PathBuf {
        self.dir.join(LOCK_FILE)
    }

    fn ticket_path(&self) -> PathBuf {
        self.dir.join(TICKET_FILE)
    }

    fn backup_path(&self) -> PathBuf {
        self.dir.join(BACKUP_FILE)
    }

    /// Persists the backup, then the ticket, each atomically. The ticket goes last so a
    /// ticket always has its backup.
    pub(crate) fn save(&self, ticket: &TicketRecord, backup: &BackupRecord) -> SysResult<()> {
        self.fs.create_dir_all(&self.dir)?;
        self.fs
            .write_atomic(&self.backup_path(), &encode(backup)?)?;
        self.fs.write_atomic(&self.ticket_path(), &encode(ticket)?)
    }

    /// Removes the ticket, then the backup. A failure to remove the **ticket** is an error:
    /// a confirmed change must not be reverted by a restart.
    pub(crate) fn clear(&self) -> SysResult<()> {
        remove_if_present(self.fs, &self.ticket_path())?;
        remove_if_present(self.fs, &self.backup_path())
    }

    /// Reads the state, classifying what is there. Never ignores damage.
    ///
    /// # Errors
    /// [`NETWORK_REVERT_CORRUPT`] when a ticket exists that nothing can be reverted from.
    pub(crate) fn load(&self) -> SysResult<Loaded> {
        let ticket_text = self.read(&self.ticket_path())?;
        let backup_text = self.read(&self.backup_path())?;
        let backup = backup_text
            .as_deref()
            .and_then(|text| serde_json::from_str::<BackupRecord>(text).ok())
            .filter(|backup| backup.version == FORMAT_VERSION);
        let Some(ticket_text) = ticket_text else {
            // No ticket: the change never started (the ticket is written before the change).
            if backup_text.is_some() {
                self.fs.remove_file(&self.backup_path())?;
            }
            return Ok(Loaded::Nothing);
        };
        let ticket = serde_json::from_str::<TicketRecord>(&ticket_text)
            .ok()
            .filter(|ticket| ticket.version == FORMAT_VERSION);
        match (ticket, backup) {
            (Some(ticket), Some(backup)) if ticket.id == backup.id => {
                Ok(Loaded::Valid(ticket, backup))
            }
            (_, Some(backup)) => Ok(Loaded::CorruptTicket(
                backup,
                "the pending network revert ticket is corrupt; reverting from its backup"
                    .to_owned(),
            )),
            (_, None) => Err(SysError::new(
                NETWORK_REVERT_CORRUPT,
                "a network change is pending but its ticket and backup are unreadable; check \
                 the machine's network configuration by hand",
            )),
        }
    }

    fn read(&self, path: &Path) -> SysResult<Option<String>> {
        if self.fs.exists(path)? {
            Ok(Some(self.fs.read_to_string(path)?))
        } else {
            Ok(None)
        }
    }

    /// The next ticket id: a persisted counter, so ids never repeat across restarts and a
    /// stale timer cannot be mistaken for a new ticket's.
    pub(crate) fn next_id(&self, now_unix: i64) -> SysResult<u64> {
        let path = self.dir.join(SEQUENCE_FILE);
        let last = self
            .read(&path)?
            .and_then(|text| text.trim().parse::<u64>().ok());
        // A missing or damaged counter restarts from the clock, which is above any id the
        // counter itself issued earlier.
        let id = match last {
            Some(last) => last.saturating_add(1),
            None => u64::try_from(now_unix).unwrap_or(0).max(1),
        };
        self.fs.create_dir_all(&self.dir)?;
        self.fs.write_atomic(&path, &id.to_string())?;
        Ok(id)
    }
}

fn encode<T: Serialize>(value: &T) -> SysResult<String> {
    serde_json::to_string(value).map_err(|e| SysError::io(format!("cannot encode state: {e}")))
}

fn remove_if_present(fs: &dyn FileSystem, path: &Path) -> SysResult<()> {
    if fs.exists(path)? {
        fs.remove_file(path)?;
    }
    Ok(())
}

/// The second, independent safety: something outside the agent process that runs the revert
/// at the deadline even if the agent is down.
pub trait RevertScheduler: Send + Sync {
    /// Arranges for the revert of ticket `id` to run in `delay_secs` seconds.
    ///
    /// # Errors
    /// [`NETWORK_REVERT_SCHEDULE`] when it cannot be arranged (the change is then not applied).
    fn schedule(&self, id: u64, delay_secs: u64) -> SysResult<()>;

    /// Cancels the scheduled revert of ticket `id`. Cancelling one that is not scheduled is
    /// not an error.
    ///
    /// # Errors
    /// A failure to run the cancel command.
    fn cancel(&self, id: u64) -> SysResult<()>;
}

/// A [`RevertScheduler`] over a transient systemd timer.
///
/// `systemd-run --on-active=<n>s <command> <id>` creates `spot-net-revert-<id>.timer`; the
/// command is the host's revert entry point (for example the agent's own binary with a
/// subcommand that calls [`crate::NetworkModule::revert_ticket`]) and receives the ticket id
/// as its last argument.
pub struct SystemdRunScheduler<'a> {
    runner: &'a dyn CommandRunner,
    command: Vec<String>,
}

impl<'a> SystemdRunScheduler<'a> {
    /// A scheduler that runs `command` (program and fixed arguments) with the ticket id
    /// appended.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] when the command is empty or names a shell.
    pub fn new(runner: &'a dyn CommandRunner, command: Vec<String>) -> SysResult<Self> {
        let Some(program) = command.first() else {
            return Err(SysError::invalid("the revert command is empty"));
        };
        crate::sys::validate_program(program)?;
        Ok(Self { runner, command })
    }

    fn unit(id: u64) -> String {
        format!("{UNIT_PREFIX}{id}")
    }
}

impl RevertScheduler for SystemdRunScheduler<'_> {
    fn schedule(&self, id: u64, delay_secs: u64) -> SysResult<()> {
        let unit = Self::unit(id);
        let spec = CommandSpec::new("systemd-run")?
            .arg(format!("--unit={unit}"))?
            .arg(format!("--on-active={}s", delay_secs.max(1)))?
            .arg("--timer-property=AccuracySec=1s")?
            .arg("--")?
            .args(self.command.iter().cloned())?
            .arg(id.to_string())?;
        let output = self.runner.run(&spec).map_err(|e| {
            SysError::new(
                NETWORK_REVERT_SCHEDULE,
                format!("the revert timer could not be scheduled: {e}"),
            )
        })?;
        output
            .require_success("scheduling the revert timer")
            .map_err(|e| {
                SysError::new(
                    NETWORK_REVERT_SCHEDULE,
                    format!("the revert timer could not be scheduled: {e}"),
                )
            })
    }

    fn cancel(&self, id: u64) -> SysResult<()> {
        let spec =
            CommandSpec::new("systemctl")?.args(["stop", &format!("{}.timer", Self::unit(id))])?;
        // A timer that is already gone (it fired, or was never made) makes stop fail; that
        // is the state we want, so a non-zero status is not an error.
        self.runner.run(&spec).map(|_| ())
    }
}
