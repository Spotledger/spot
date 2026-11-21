//! Target selection for `spot backup` (WP-PRD-04.T13): one place that turns the `[backup.targets]`
//! entry a command names into the directory the run reads and writes, for every kind.
//!
//! - `folder`: the configured path, as before.
//! - `removable`: the volume is named by label or UUID (never a drive letter or a path). It is
//!   looked up through the device table, refused when it is not removable, shares a disk with the
//!   system, or matches twice (`backup.target_unsafe`, `backup.target_ambiguous`), and when it is
//!   not plugged in or not mounted the run is refused with `backup.target_absent` (nothing
//!   written, retry at the next slot). The directory used is the volume's mount point plus
//!   `subdir` (default `spot-backups`), created when missing.
//! - `cloud`: needs a connector (an S3-compatible transport) and the tenant key, which the host
//!   supplies; none is configured on this host, so it is refused with `backup.target_unavailable`
//!   naming no endpoint, key or credential. A real connector waits for a decision on the HTTP
//!   stack (see the task report).
//!
//! The device table is a parameter so the tests drive it with a scripted fake; the command passes
//! the platform's own ([`SystemDevices`]).

use std::path::PathBuf;
use std::sync::Arc;

use spot_cli::config::BackupTarget as Configured;
use spot_cli::services::backup_targets::{
    DEFAULT_SUBDIR, DeviceTable, SelectedVolume, SystemDevices, TARGET_ABSENT, TargetError,
    VolumeSelector, eject_hint,
};

/// Why a target cannot be used, as the code and a sentence the command prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The stable `backup.*` code.
    pub code: &'static str,
    /// A sentence with no secret, endpoint or credential.
    pub reason: String,
    /// Whether the next scheduled slot may succeed (an unplugged drive).
    pub retry_next_slot: bool,
}

impl From<TargetError> for Refusal {
    fn from(error: TargetError) -> Self {
        Self {
            code: error.code(),
            reason: error.reason().to_owned(),
            retry_next_slot: error.code() == TARGET_ABSENT,
        }
    }
}

/// The directory a run uses, and what to tell the operator afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The directory holding the packages and the ledger.
    pub dir: PathBuf,
    /// The line to print after an interactive run (a removable drive): how to eject it.
    pub eject_hint: Option<String>,
}

/// The device table the command uses: the platform's own.
pub fn system_devices() -> Arc<dyn DeviceTable> {
    Arc::new(SystemDevices)
}

/// Resolves `target` to its directory.
///
/// # Errors
///
/// A [`Refusal`] with the stable code; `backup.target_absent` carries `retry_next_slot`.
pub fn resolve(target: &Configured, devices: Arc<dyn DeviceTable>) -> Result<Resolved, Refusal> {
    match target {
        Configured::Folder { path } => Ok(Resolved {
            dir: path.clone(),
            eject_hint: None,
        }),
        Configured::Removable { volume, subdir } => {
            let selector = VolumeSelector::parse(volume)?;
            let selected = SelectedVolume::new(devices, selector);
            let assessed = selected.assess()?;
            let subdir = subdir.clone().unwrap_or_else(|| DEFAULT_SUBDIR.to_owned());
            let dir = assessed.volume.mount_point.join(&subdir);
            // A bounded creation inside the mounted drive; a failure is the drive's, not ours.
            std::fs::create_dir_all(&dir).map_err(|_| Refusal {
                code: "backup.target_unavailable",
                reason: "the backup folder on the drive could not be created".to_owned(),
                retry_next_slot: false,
            })?;
            Ok(Resolved {
                dir,
                eject_hint: Some(eject_hint(&assessed.device)),
            })
        }
        Configured::Cloud { .. } => Err(Refusal {
            code: "backup.target_unavailable",
            reason: "a cloud target needs a connector and the tenant key that the host supplies; none is configured on this host"
                .to_owned(),
            retry_next_slot: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_folder_resolves_to_its_path_without_a_device_lookup() {
        let target = Configured::Folder {
            path: PathBuf::from("/srv/backups"),
        };
        let resolved = resolve(&target, system_devices()).unwrap();
        assert_eq!(resolved.dir, PathBuf::from("/srv/backups"));
        assert!(resolved.eject_hint.is_none());
    }
}
