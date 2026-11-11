//! Removable-volume discovery: the platform probe, its Windows/Linux real
//! implementations and the fake every test drives.
//!
//! The probe answers one question — which volumes exist, whether they are removable,
//! which physical disk backs them and how much room they have. It never mounts,
//! formats or writes. [`crate::target::RemovableTarget`] decides what to do with that.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{BackupError, Result};

/// One volume the platform reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// The platform's id: `/dev/sdb1` on Linux, `E:` on Windows.
    pub id: String,
    /// A human label when the platform knows one.
    pub label: String,
    /// Where it is mounted.
    pub mount_point: PathBuf,
    /// Whether the platform calls the device removable.
    pub removable: bool,
    /// The physical disk that backs the volume (`sdb`, or a canonical device path).
    pub physical_disk: String,
    /// Whether the media is inserted. A probe may keep a known volume with
    /// `present = false`; a real probe simply omits absent volumes.
    pub present: bool,
    /// Free bytes, integer.
    pub free_space: u64,
}

/// Lists the volumes the platform currently sees.
pub trait VolumeProbe: Send + Sync {
    /// Every volume, including non-removable disks (the caller filters).
    ///
    /// # Errors
    /// [`BackupError::Unsupported`] when the platform has no probe, or an I/O error
    /// from the system query.
    fn volumes(&self) -> Result<Vec<Volume>>;
}

/// Only the removable, present volumes from `probe`.
///
/// # Errors
/// Propagates the probe's error.
pub fn volumes_for_probe(probe: &dyn VolumeProbe) -> Result<Vec<Volume>> {
    Ok(probe
        .volumes()?
        .into_iter()
        .filter(|volume| volume.removable && volume.present)
        .collect())
}

/// A scripted probe for tests and for a host whose platform adapter is not wired yet.
#[derive(Debug, Default)]
pub struct FakeVolumeProbe {
    volumes: Mutex<Vec<Volume>>,
}

impl FakeVolumeProbe {
    /// A probe with no volumes.
    pub fn new() -> FakeVolumeProbe {
        FakeVolumeProbe::default()
    }

    /// A probe holding `volumes`.
    pub fn with(volumes: Vec<Volume>) -> FakeVolumeProbe {
        FakeVolumeProbe {
            volumes: Mutex::new(volumes),
        }
    }

    /// Replaces the probe's volumes.
    ///
    /// # Errors
    /// [`BackupError::io`] when the test mutex is poisoned.
    pub fn set(&self, volumes: Vec<Volume>) -> Result<()> {
        let mut guard = self
            .volumes
            .lock()
            .map_err(|_| BackupError::io("the fake volume probe is poisoned"))?;
        *guard = volumes;
        Ok(())
    }

    /// Marks the volume with `id` present or absent, keeping the other fields.
    ///
    /// # Errors
    /// [`BackupError::NotFound`] when no such volume is known.
    pub fn set_present(&self, id: &str, present: bool) -> Result<()> {
        let mut guard = self
            .volumes
            .lock()
            .map_err(|_| BackupError::io("the fake volume probe is poisoned"))?;
        let volume = guard
            .iter_mut()
            .find(|volume| volume.id == id)
            .ok_or_else(|| BackupError::NotFound { id: id.to_owned() })?;
        volume.present = present;
        Ok(())
    }
}

impl VolumeProbe for FakeVolumeProbe {
    fn volumes(&self) -> Result<Vec<Volume>> {
        let guard = self
            .volumes
            .lock()
            .map_err(|_| BackupError::io("the fake volume probe is poisoned"))?;
        Ok(guard.clone())
    }
}

/// The platform's own probe.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemVolumeProbe;

impl VolumeProbe for SystemVolumeProbe {
    fn volumes(&self) -> Result<Vec<Volume>> {
        system_volumes()
    }
}

#[cfg(target_os = "linux")]
fn system_volumes() -> Result<Vec<Volume>> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| BackupError::io(format!("reading /proc/self/mountinfo: {error}")))?;
    let mut by_mount: BTreeMap<PathBuf, Volume> = BTreeMap::new();
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        if fields.len() <= separator + 2 || fields.len() < 5 {
            continue;
        }
        let mount_point = PathBuf::from(unescape_mountinfo(fields[4]));
        let source = fields[separator + 2];
        let Some(device) = source.strip_prefix("/dev/") else {
            continue;
        };
        let base = Path::new(device)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| device.to_owned());
        let (removable, physical_disk) = disk_facts(&base);
        let free_space = free_space_of(&mount_point).unwrap_or(0);
        by_mount.insert(
            mount_point.clone(),
            Volume {
                id: source.to_owned(),
                label: source.to_owned(),
                mount_point,
                removable,
                physical_disk,
                present: true,
                free_space,
            },
        );
    }
    Ok(by_mount.into_values().collect())
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo(text: &str) -> String {
    text.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(target_os = "linux")]
fn disk_facts(base: &str) -> (bool, String) {
    let canonical = std::fs::canonicalize(format!("/sys/class/block/{base}"))
        .unwrap_or_else(|_| PathBuf::from(format!("/sys/class/block/{base}")));
    let components: Vec<String> = canonical
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let disk = components
        .iter()
        .position(|component| component == "block")
        .and_then(|index| components.get(index + 1))
        .cloned()
        .unwrap_or_else(|| base.to_owned());
    let removable = std::fs::read_to_string(format!("/sys/class/block/{disk}/removable"))
        .map(|text| text.trim() == "1")
        .unwrap_or(false);
    (removable, disk)
}

#[cfg(target_os = "linux")]
fn free_space_of(path: &Path) -> Result<u64> {
    let output = std::process::Command::new("df")
        .args(["-P", "-B1", "--"])
        .arg(path)
        .output()
        .map_err(|error| BackupError::io(format!("free space of {}: {error}", path.display())))?;
    if !output.status.success() {
        return Err(BackupError::io(format!(
            "free space of {}: df failed",
            path.display()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let field = text
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().nth(3))
        .ok_or_else(|| BackupError::io("df output was not understood"))?;
    field
        .parse()
        .map_err(|_| BackupError::io(format!("df reported {field:?}")))
}

#[cfg(target_os = "windows")]
fn system_volumes() -> Result<Vec<Volume>> {
    let script = "Get-CimInstance Win32_LogicalDisk | Select-Object DeviceID,VolumeName,DriveType,Size,FreeSpace | ConvertTo-Json -Compress";
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|error| BackupError::io(format!("reading Windows volumes: {error}")))?;
    if !output.status.success() {
        return Err(BackupError::io(
            "reading Windows volumes: powershell failed",
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|error| BackupError::io(format!("reading Windows volumes: {error}")))?;
    let items = match parsed {
        serde_json::Value::Array(items) => items,
        value => vec![value],
    };
    let mut out = Vec::new();
    for item in items {
        let Some(id) = item.get("DeviceID").and_then(|value| value.as_str()) else {
            continue;
        };
        let drive_type = item
            .get("DriveType")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let free_space = item
            .get("FreeSpace")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let label = item
            .get("VolumeName")
            .and_then(|value| value.as_str())
            .unwrap_or(id)
            .to_owned();
        out.push(Volume {
            id: id.to_owned(),
            label,
            mount_point: PathBuf::from(format!("{id}\\")),
            removable: drive_type == 2,
            physical_disk: id.to_owned(),
            present: true,
            free_space,
        });
    }
    Ok(out)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn system_volumes() -> Result<Vec<Volume>> {
    Err(BackupError::Unsupported {
        feature: "removable-volume discovery on this platform; supply a VolumeProbe".to_owned(),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_can_take_media_away_and_put_it_back() {
        let probe = FakeVolumeProbe::with(vec![Volume {
            id: "/dev/sdb1".to_owned(),
            label: "BACKUP".to_owned(),
            mount_point: "/media/backup".into(),
            removable: true,
            physical_disk: "sdb".to_owned(),
            present: true,
            free_space: 10_000,
        }]);
        assert_eq!(volumes_for_probe(&probe).expect("list").len(), 1);
        probe.set_present("/dev/sdb1", false).expect("absent");
        assert!(volumes_for_probe(&probe).expect("list").is_empty());
        probe.set_present("/dev/sdb1", true).expect("present");
        assert_eq!(volumes_for_probe(&probe).expect("list").len(), 1);
    }
}
