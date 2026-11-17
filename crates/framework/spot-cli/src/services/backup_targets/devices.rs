//! Removable-drive discovery for `backup_targets` (WP-PRD-04.T5): the device table behind a
//! trait, its Linux and Windows readers, and the one configured volume with every safety rule.

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use spot_backup::{BackupError, Volume, VolumeProbe};
#[cfg(target_os = "linux")]
use spot_backup::{SpaceProbe, SystemSpaceProbe};

use super::{
    BResult, Outcome, TARGET_ABSENT, TARGET_AMBIGUOUS, TARGET_UNSAFE, TARGET_UNSUPPORTED,
    TargetError, VolumeSelector,
};

// ---------------------------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------------------------

/// One block device or volume the platform reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    /// The platform's name: `/dev/sdb1` or `E:`.
    pub device: String,
    /// The file-system UUID (Windows: the volume serial number).
    pub uuid: Option<String>,
    /// The volume label.
    pub label: Option<String>,
    /// Whether the platform calls the device removable.
    pub removable: bool,
    /// The physical disk behind the volume (`sdb`, or `E:` on Windows).
    pub physical_disk: String,
    /// Where it is mounted; `None` when it is plugged in but not mounted.
    pub mount_point: Option<PathBuf>,
    /// Free bytes.
    pub free_bytes: u64,
}

/// What the platform reports at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceSnapshot {
    /// Every device with a UUID or a label.
    pub devices: Vec<DeviceEntry>,
    /// The physical disks that hold the operating system (`/`, `/boot`, `/home`, ...).
    pub system_disks: Vec<String>,
}

/// Lists the devices; never mounts, formats or writes.
pub trait DeviceTable: Send + Sync {
    /// The devices now.
    ///
    /// # Errors
    /// `backup.unsupported` when the platform has no probe; `backup.target_unsafe` is never
    /// returned here.
    fn snapshot(&self) -> Outcome<DeviceSnapshot>;
}

/// A scripted table for tests and for hosts without a platform probe.
#[derive(Debug, Default)]
pub struct FakeDevices {
    snapshot: Mutex<DeviceSnapshot>,
}

impl FakeDevices {
    /// A table holding `snapshot`.
    pub fn new(snapshot: DeviceSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
        }
    }

    /// Replaces what the table reports (plug, unplug, mount).
    pub fn set(&self, snapshot: DeviceSnapshot) {
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
    }
}

impl DeviceTable for FakeDevices {
    fn snapshot(&self) -> Outcome<DeviceSnapshot> {
        Ok(self
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }
}

/// The platform's own table: Linux `/dev/disk` and `/proc/self/mountinfo`, Windows
/// `Win32_LogicalDisk` through PowerShell (an argument vector, never a shell string).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemDevices;

impl DeviceTable for SystemDevices {
    fn snapshot(&self) -> Outcome<DeviceSnapshot> {
        system_snapshot()
    }
}

/// One line of `/proc/self/mountinfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    /// The mount source, for example `/dev/sdb1`.
    pub source: String,
    /// The mount point, with the kernel's octal escapes decoded.
    pub mount_point: PathBuf,
    /// The file-system type.
    pub fstype: String,
}

/// Decodes the kernel's `\040`-style octal escapes.
pub fn unescape_octal(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = |b: u8| (b'0'..=b'7').contains(&b);
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && octal(bytes[i + 1])
            && octal(bytes[i + 2])
            && octal(bytes[i + 3])
        {
            let value = (u32::from(bytes[i + 1] - b'0') << 6)
                | (u32::from(bytes[i + 2] - b'0') << 3)
                | u32::from(bytes[i + 3] - b'0');
            out.push(u8::try_from(value & 0xff).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decodes udev's `\x20`-style hexadecimal escapes in `/dev/disk/by-label` names.
pub fn unescape_udev(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1] == b'x'
            && bytes[i + 2].is_ascii_hexdigit()
            && bytes[i + 3].is_ascii_hexdigit()
        {
            let pair = std::str::from_utf8(&bytes[i + 2..i + 4]).unwrap_or("3f");
            out.push(u8::from_str_radix(pair, 16).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mounts in `/proc/self/mountinfo` text; lines that do not parse are skipped.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(dash) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        if fields.len() < 5 || fields.len() < dash + 3 {
            continue;
        }
        out.push(MountEntry {
            source: fields[dash + 2].to_owned(),
            mount_point: PathBuf::from(unescape_octal(fields[4])),
            fstype: fields[dash + 1].to_owned(),
        });
    }
    out
}

/// The physical disk of a block device from its canonical `/sys` path: the component after
/// `block` (`.../block/sdb/sdb1` gives `sdb`), or `base` when the path has none.
pub fn physical_disk_of(sys_path: &str, base: &str) -> String {
    let parts: Vec<&str> = sys_path.split('/').collect();
    parts
        .iter()
        .position(|p| *p == "block")
        .and_then(|i| parts.get(i + 1))
        .map_or_else(|| base.to_owned(), |disk| (*disk).to_owned())
}

/// The devices of `Win32_LogicalDisk` lines `DeviceID|DriveType|VolumeName|VolumeSerialNumber|FreeSpace`.
/// Drive type 2 is `DRIVE_REMOVABLE`. `system_drive` (`C:`) is the system disk.
pub fn parse_logical_disks(text: &str, system_drive: &str) -> DeviceSnapshot {
    let mut devices = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.trim().split('|').collect();
        if f.len() != 5 || f[0].len() != 2 || !f[0].ends_with(':') {
            continue;
        }
        let Ok(kind) = f[1].trim().parse::<u32>() else {
            continue;
        };
        let id = f[0].to_ascii_uppercase();
        devices.push(DeviceEntry {
            mount_point: Some(PathBuf::from(format!("{id}\\"))),
            physical_disk: id.clone(),
            device: id,
            uuid: Some(f[3].trim().to_owned()).filter(|s| !s.is_empty()),
            label: Some(f[2].trim().to_owned()).filter(|s| !s.is_empty()),
            removable: kind == 2,
            free_bytes: f[4].trim().parse::<u64>().unwrap_or(0),
        });
    }
    DeviceSnapshot {
        devices,
        system_disks: vec![system_drive.to_ascii_uppercase()],
    }
}

#[cfg(target_os = "linux")]
fn system_snapshot() -> Outcome<DeviceSnapshot> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|_| TargetError::new(TARGET_UNSUPPORTED, "the mount table cannot be read"))?;
    let mounts = parse_mountinfo(&text);
    let canonical = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
    let mut devices: BTreeMap<PathBuf, DeviceEntry> = BTreeMap::new();
    let facts = |device: &Path| -> (bool, String) {
        let base = device
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let sys = std::fs::canonicalize(format!("/sys/class/block/{base}"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let disk = physical_disk_of(&sys, &base);
        let removable = std::fs::read_to_string(format!("/sys/class/block/{disk}/removable"))
            .is_ok_and(|t| t.trim() == "1");
        (removable, disk)
    };
    for (dir, is_uuid) in [("/dev/disk/by-uuid", true), ("/dev/disk/by-label", false)] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(device) = std::fs::canonicalize(entry.path()) else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let (removable, physical_disk) = facts(&device);
            let slot = devices
                .entry(device.clone())
                .or_insert_with(|| DeviceEntry {
                    device: device.to_string_lossy().into_owned(),
                    uuid: None,
                    label: None,
                    removable,
                    physical_disk,
                    mount_point: None,
                    free_bytes: 0,
                });
            if is_uuid {
                slot.uuid = Some(name);
            } else {
                slot.label = Some(unescape_udev(&name));
            }
        }
    }
    for entry in devices.values_mut() {
        let wanted = PathBuf::from(&entry.device);
        if let Some(mount) = mounts
            .iter()
            .find(|m| m.source.starts_with("/dev/") && canonical(&m.source) == wanted)
        {
            entry.free_bytes = SystemSpaceProbe.free_bytes(&mount.mount_point).unwrap_or(0);
            entry.mount_point = Some(mount.mount_point.clone());
        }
    }
    let system_mounts = ["/", "/boot", "/boot/efi", "/home", "/var", "/usr"];
    let mut system_disks: Vec<String> = mounts
        .iter()
        .filter(|m| m.source.starts_with("/dev/"))
        .filter(|m| system_mounts.iter().any(|s| m.mount_point == Path::new(s)))
        .map(|m| facts(&canonical(&m.source)).1)
        .collect();
    system_disks.sort();
    system_disks.dedup();
    Ok(DeviceSnapshot {
        devices: devices.into_values().collect(),
        system_disks,
    })
}

#[cfg(windows)]
fn system_snapshot() -> Outcome<DeviceSnapshot> {
    let script = "Get-CimInstance Win32_LogicalDisk | ForEach-Object { '{0}|{1}|{2}|{3}|{4}' -f $_.DeviceID,$_.DriveType,$_.VolumeName,$_.VolumeSerialNumber,$_.FreeSpace }";
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|_| TargetError::new(TARGET_UNSUPPORTED, "the volume list cannot be read"))?;
    if !output.status.success() {
        return Err(TargetError::new(
            TARGET_UNSUPPORTED,
            "the volume list cannot be read",
        ));
    }
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
    Ok(parse_logical_disks(
        &String::from_utf8_lossy(&output.stdout),
        &system,
    ))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn system_snapshot() -> Outcome<DeviceSnapshot> {
    Err(TargetError::new(
        TARGET_UNSUPPORTED,
        "removable drives are not supported on this platform",
    ))
}

// ---------------------------------------------------------------------------------------------
// The selected volume
// ---------------------------------------------------------------------------------------------

/// A volume that passed every rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assessed {
    /// The volume as `RemovableTarget` sees it (id is the selector id).
    pub volume: Volume,
    /// The platform's device name.
    pub device: String,
    /// The volume's UUID when the platform reports one: record this in an audit line, not the
    /// label.
    pub uuid: Option<String>,
}

/// One configured volume over a [`DeviceTable`]; also the `VolumeProbe` of `RemovableTarget`.
pub struct SelectedVolume {
    table: Arc<dyn DeviceTable>,
    selector: VolumeSelector,
}

impl SelectedVolume {
    /// The volume `selector` names, looked up through `table`.
    pub fn new(table: Arc<dyn DeviceTable>, selector: VolumeSelector) -> Self {
        Self { table, selector }
    }

    /// The selector.
    pub fn selector(&self) -> &VolumeSelector {
        &self.selector
    }

    /// Finds the volume and applies every rule.
    ///
    /// # Errors
    /// `backup.target_absent` when it is not plugged in or not mounted, `backup.target_ambiguous`
    /// when several match, `backup.target_unsafe` when it is not removable or shares a disk with
    /// the system.
    pub fn assess(&self) -> Outcome<Assessed> {
        let snapshot = self.table.snapshot()?;
        let id = self.selector.id();
        let mut found = snapshot.devices.iter().filter(|d| self.selector.matches(d));
        let (Some(entry), None) = (found.next(), found.next()) else {
            let count = snapshot
                .devices
                .iter()
                .filter(|d| self.selector.matches(d))
                .count();
            return Err(if count == 0 {
                TargetError::new(TARGET_ABSENT, format!("the volume {id} is not plugged in"))
            } else {
                TargetError::new(
                    TARGET_AMBIGUOUS,
                    format!("{count} volumes match {id}; name it by UUID"),
                )
            });
        };
        let Some(mount_point) = entry.mount_point.clone() else {
            return Err(TargetError::new(
                TARGET_ABSENT,
                format!("the volume {id} is plugged in but not mounted"),
            ));
        };
        if !entry.removable {
            return Err(TargetError::new(
                TARGET_UNSAFE,
                format!("the volume {id} is not a removable device"),
            ));
        }
        if snapshot.system_disks.contains(&entry.physical_disk) {
            return Err(TargetError::new(
                TARGET_UNSAFE,
                format!("the volume {id} is on the system disk"),
            ));
        }
        Ok(Assessed {
            volume: Volume {
                id,
                label: entry.label.clone().unwrap_or_else(|| entry.device.clone()),
                mount_point,
                removable: true,
                physical_disk: entry.physical_disk.clone(),
                present: true,
                free_space: entry.free_bytes,
            },
            device: entry.device.clone(),
            uuid: entry.uuid.clone(),
        })
    }
}

impl VolumeProbe for SelectedVolume {
    fn volumes(&self) -> BResult<Vec<Volume>> {
        match self.assess() {
            Ok(found) => Ok(vec![found.volume]),
            Err(error) if error.code() == TARGET_ABSENT => Ok(Vec::new()),
            Err(error) => Err(BackupError::TargetUnavailable {
                target: self.selector.id(),
                reason: error.to_string(),
            }),
        }
    }
}

/// What to tell the operator after an interactive run on a removable drive.
pub fn eject_hint(device: &str) -> String {
    if cfg!(windows) {
        format!("Safe to remove: use \"Safely Remove Hardware\" for {device} before unplugging it.")
    } else {
        format!("Safe to remove: run `udisksctl unmount -b {device}` and then unplug it.")
    }
}
