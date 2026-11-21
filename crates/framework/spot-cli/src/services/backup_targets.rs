//! Backup targets beyond a folder: a removable drive (USB) and a cloud object store
//! (WP-PRD-04.T5). Guide: `docs/running-spot/backup_targets.md`.
//!
//! `spot-backup` owns the [`BackupTarget`] port and the generic `RemovableTarget` and
//! `CloudTarget`. This module supplies what they need on a real machine and wraps each kind in a
//! [`TargetAdapter`] that a command can pick by name from a [`TargetRegistry`]:
//!
//! - **removable:** the configuration names a volume by label or UUID (`volume = "SPOT-BACKUP"`),
//!   never by drive letter or mount path. [`DeviceTable`] says which block devices exist, behind a
//!   trait so every rule is tested with [`FakeDevices`] and no hardware. The real table reads
//!   `/dev/disk/by-uuid`, `/dev/disk/by-label` and `/proc/self/mountinfo` on Linux and
//!   `Win32_LogicalDisk` (drive type 2, the `DRIVE_REMOVABLE` of `GetDriveType`) on Windows. A
//!   volume that is not mounted is absent (`backup.target_absent`: alert, retry next slot), never a
//!   write into the empty mount-point directory; a volume that is not removable, or that shares
//!   its disk with the system, is refused (`backup.target_unsafe`); two volumes with the same
//!   label are refused (`backup.target_ambiguous`).
//! - **cloud:** [`ChunkedCloudStore`] seals every 4 MiB chunk under the tenant's own key (a
//!   `spot_cloud_backup::RecoveryKey`, never sent anywhere), uploads it through the
//!   `spot_cloud_backup::ObjectStore` port, reads each chunk back and compares digests, and
//!   writes a sealed manifest last, so a killed upload is resumed chunk by chunk and never listed
//!   half done. The S3 transport is a [`CloudConnector`] the host supplies; there is no `insecure`
//!   option and the credential is read through a [`CredentialSource`] from a secret reference.
//! - **restore:** [`TargetAdapter::fetch`] opens a package for `spot restore`, refusing a package
//!   of another tenant and checking digests (removable: the package's own quick verification;
//!   cloud: chunk digests, authentication tags and the whole-package digest).
//!
//! Nothing here reads the wall clock, uses `unsafe`, or puts a key, a credential or package bytes
//! in an error. There is no `register` function: this file is a plain helper module until the host
//! wires `[backup.targets]` into [`TargetRegistry::new`] (see the task report).

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use spot_backup::package::{MAGIC, PACKAGE_EXTENSION};
use spot_backup::{
    BackupError, BackupManifest, BackupTarget, CloudStore, CloudTarget, FolderTarget,
    RemovableTarget, SpaceProbe, StoredBackup, SystemSpaceProbe, VolumeProbe,
};
use spot_cloud_backup::RecoveryKey;

// The services directory is scanned flat by build.rs, so the module paths are spelled out.
#[path = "backup_targets/cloud.rs"]
mod cloud;
#[path = "backup_targets/devices.rs"]
mod devices;

pub use cloud::{
    ChunkedCloudStore, CloudConnector, Credential, CredentialSource, throttle_delay_ms,
};
pub use devices::{
    Assessed, DeviceEntry, DeviceSnapshot, DeviceTable, FakeDevices, MountEntry, SelectedVolume,
    SystemDevices, eject_hint, parse_logical_disks, parse_mountinfo, physical_disk_of,
    unescape_octal, unescape_udev,
};

/// The volume is not plugged in or not mounted: alert, retry at the next slot.
pub const TARGET_ABSENT: &str = "backup.target_absent";
/// The volume is a system disk, not removable, or otherwise unsafe to write.
pub const TARGET_UNSAFE: &str = "backup.target_unsafe";
/// More than one volume matches the configured label or UUID.
pub const TARGET_AMBIGUOUS: &str = "backup.target_ambiguous";
/// The target's configuration is wrong.
pub const TARGET_INVALID: &str = "backup.target_invalid";
/// A package belongs to another tenant.
pub const TARGET_FOREIGN: &str = "backup.target_foreign_tenant";
/// No such package or target.
pub const TARGET_NOT_FOUND: &str = "backup.not_found";
/// This kind of target cannot be used here (no connector, no key, no platform probe).
pub const TARGET_UNSUPPORTED: &str = "backup.unsupported";

/// The default directory below a removable volume's mount point.
pub const DEFAULT_SUBDIR: &str = "spot-backups";
/// The most bytes of a package header this module will read.
const MAX_HEADER: u64 = 16 * 1024 * 1024;

type Outcome<T> = std::result::Result<T, TargetError>;
type BResult<T> = std::result::Result<T, BackupError>;

/// Why a target was refused or failed: a stable code and a reason that never holds a key, a
/// credential or package bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetError {
    code: &'static str,
    reason: String,
}

impl TargetError {
    /// An error with `code` and `reason`.
    pub fn new(code: &'static str, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }

    /// The stable code, for example `backup.target_absent`.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The reason, in plain language.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.reason)
    }
}

impl std::error::Error for TargetError {}

impl From<BackupError> for TargetError {
    fn from(error: BackupError) -> Self {
        Self::new(error.code(), error.reason())
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// One configured target, by kind. The `kind` key selects the variant; unknown keys are refused.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetSpec {
    /// A directory on this machine.
    Folder {
        /// Absolute path of the directory.
        path: PathBuf,
    },
    /// A removable drive named by label or UUID.
    Removable {
        /// `uuid:<id>`, `label:<name>`, or a bare 36-character UUID, `XXXX-XXXX` or label. Never
        /// a drive letter or a path.
        volume: String,
        /// The directory below the mount point (default [`DEFAULT_SUBDIR`]).
        #[serde(default)]
        subdir: Option<String>,
    },
    /// An S3-compatible object store.
    Cloud {
        /// The store's `https://` endpoint, without credentials.
        endpoint: String,
        /// An identifier of the access key, never key material.
        key_id: String,
        /// A reference into the secret store (`secret:<name>`) holding the access secret.
        credential_ref: String,
        /// The prefix below the bucket (default none).
        #[serde(default)]
        prefix: Option<String>,
        /// Upload bandwidth limit in KiB per second (default unlimited).
        #[serde(default)]
        max_kbps: Option<u32>,
        /// The bytes the account may hold, when the store cannot say (default unlimited).
        #[serde(default)]
        quota_bytes: Option<u64>,
    },
}

fn segment_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text != "."
        && text != ".."
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn relative_ok(text: &str) -> bool {
    !text.is_empty() && text.len() <= 200 && text.split('/').all(segment_ok)
}

impl TargetSpec {
    /// The rules this target breaks, without echoing a secret. Empty means usable.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            Self::Folder { path } => {
                if !path.is_absolute()
                    || path
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    out.push("path: must be absolute with no '..' component".to_owned());
                }
            }
            Self::Removable { volume, subdir } => {
                if let Err(error) = VolumeSelector::parse(volume) {
                    out.push(format!("volume: {}", error.reason()));
                }
                if let Some(subdir) = subdir
                    && !relative_ok(subdir)
                {
                    out.push(
                        "subdir: must be a relative path of letters, digits, '-', '_', '.'"
                            .to_owned(),
                    );
                }
            }
            Self::Cloud {
                endpoint,
                key_id,
                credential_ref,
                prefix,
                max_kbps,
                ..
            } => {
                let authority = endpoint
                    .strip_prefix("https://")
                    .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or(""));
                match authority {
                    None => out.push("endpoint: must be an https:// URL".to_owned()),
                    Some(a) if a.is_empty() || a.contains('@') => {
                        out.push("endpoint: must have a host and no userinfo".to_owned());
                    }
                    Some(_) => {}
                }
                if endpoint.contains(['?', '#']) || endpoint.chars().any(char::is_whitespace) {
                    out.push(
                        "endpoint: must not contain a query, fragment or whitespace".to_owned(),
                    );
                }
                if key_id.is_empty() || key_id.len() > 128 {
                    out.push("key_id: must be 1 to 128 bytes".to_owned());
                }
                let reference = credential_ref.strip_prefix("secret:").unwrap_or("");
                if reference.is_empty()
                    || reference.len() > 128
                    || reference
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control())
                {
                    out.push("credential_ref: must be a secret reference (secret:<name>), never the secret".to_owned());
                }
                if let Some(prefix) = prefix
                    && !prefix.is_empty()
                    && !relative_ok(prefix)
                {
                    out.push(
                        "prefix: must be a relative path of letters, digits, '-', '_', '.'"
                            .to_owned(),
                    );
                }
                if *max_kbps == Some(0) {
                    out.push("max_kbps: must be at least 1".to_owned());
                }
            }
        }
        out
    }
}

/// How a removable volume is named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeSelector {
    /// A file-system UUID (Windows: the volume serial number), compared without case.
    Uuid(String),
    /// A volume label, compared exactly.
    Label(String),
}

fn hex_only(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The shapes taken for a UUID when the text has no prefix: 8-4-4-4-12, `XXXX-XXXX` (FAT) or 16
/// hex digits (NTFS). A bare 8-digit serial needs the `uuid:` prefix, so a label like `BADCAFE0`
/// is never mistaken for one.
fn uuid_shaped(text: &str) -> bool {
    let dashed = |parts: &[usize]| {
        let pieces: Vec<&str> = text.split('-').collect();
        pieces.len() == parts.len()
            && pieces
                .iter()
                .zip(parts)
                .all(|(p, n)| p.len() == *n && hex_only(p))
    };
    dashed(&[8, 4, 4, 4, 12]) || dashed(&[4, 4]) || (text.len() == 16 && hex_only(text))
}

impl VolumeSelector {
    /// Parses a configured `volume` value.
    ///
    /// # Errors
    /// `backup.target_invalid` for an empty value, a drive letter (`E:`), a path, a control
    /// character, or a `uuid:` value that is not hexadecimal.
    pub fn parse(text: &str) -> Outcome<Self> {
        let bad = |why: &str| Err(TargetError::new(TARGET_INVALID, why));
        let text = text.trim();
        let drive_letter = |t: &str| {
            let b = t.as_bytes();
            b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
        };
        if drive_letter(text) {
            return bad("name the volume by label or UUID, not by drive letter");
        }
        let (explicit, body) = if let Some(rest) = text.strip_prefix("uuid:") {
            (Some(true), rest)
        } else if let Some(rest) = text.strip_prefix("label:") {
            (Some(false), rest)
        } else {
            (None, text)
        };
        if body.is_empty() || body.len() > 64 {
            return bad("the volume must be 1 to 64 bytes");
        }
        if body
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
        {
            return bad("name the volume by label or UUID, not by a path");
        }
        if drive_letter(body) {
            return bad("name the volume by label or UUID, not by drive letter");
        }
        let is_uuid = explicit.unwrap_or_else(|| uuid_shaped(body));
        if is_uuid {
            if body.len() < 8 || !body.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                return bad("a uuid: volume must be 8 to 64 hexadecimal digits and dashes");
            }
            Ok(Self::Uuid(body.to_ascii_uppercase()))
        } else {
            Ok(Self::Label(body.to_owned()))
        }
    }

    /// The canonical id: `uuid:<UPPER>` or `label:<name>`.
    pub fn id(&self) -> String {
        match self {
            Self::Uuid(u) => format!("uuid:{u}"),
            Self::Label(l) => format!("label:{l}"),
        }
    }

    fn matches(&self, entry: &DeviceEntry) -> bool {
        match self {
            Self::Uuid(u) => entry
                .uuid
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(u)),
            Self::Label(l) => entry.label.as_deref() == Some(l.as_str()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Adapters
// ---------------------------------------------------------------------------------------------

/// One target a command or the scheduled service can use: the `BackupTarget` the runner writes to
/// plus the checks, the restore read and the hint around it.
pub trait TargetAdapter: Send + Sync {
    /// The configured name.
    fn name(&self) -> &str;
    /// `folder`, `removable` or `cloud`.
    fn kind(&self) -> &'static str;
    /// The target the runner writes to.
    fn target(&self) -> &dyn BackupTarget;
    /// Whether a backup can be written now.
    ///
    /// # Errors
    /// `backup.target_absent` (retry next slot), `backup.target_unsafe`, `backup.target_ambiguous`.
    fn preflight(&self) -> Outcome<()>;
    /// The backups on the target, newest first.
    ///
    /// # Errors
    /// The preflight errors, or `backup.io`.
    fn list(&self) -> Outcome<Vec<StoredBackup>> {
        self.preflight()?;
        Ok(self.target().list()?)
    }
    /// Opens backup `id` for restore. A package of another tenant is refused
    /// (`backup.target_foreign_tenant`); digests are checked.
    ///
    /// # Errors
    /// `backup.not_found`, `backup.target_foreign_tenant`, `backup.verify_failed` and the
    /// preflight errors.
    fn fetch(&self, id: &str, expected_tenant: &str) -> Outcome<Box<dyn Read + Send>>;
    /// The line to print after an interactive run, for a removable drive.
    fn eject_hint(&self) -> Option<String> {
        None
    }
}

fn id_ok(id: &str) -> Outcome<()> {
    if id.is_empty()
        || id.len() > 200
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(TargetError::new(
            TARGET_NOT_FOUND,
            "the backup id is not valid",
        ));
    }
    Ok(())
}

/// Reads the package header from `reader`, refuses another tenant's package, and returns a reader
/// that still yields the whole package.
fn tenant_checked(
    mut reader: Box<dyn Read + Send>,
    expected_tenant: &str,
) -> Outcome<Box<dyn Read + Send>> {
    let header = |_: std::io::Error| {
        TargetError::new("backup.verify_failed", "the package header cannot be read")
    };
    let mut head = [0u8; 16];
    reader.read_exact(&mut head).map_err(header)?;
    let mut length = [0u8; 8];
    length.copy_from_slice(&head[8..16]);
    let length = u64::from_le_bytes(length);
    if &head[..8] != MAGIC || length == 0 || length > MAX_HEADER {
        return Err(TargetError::new(
            "backup.verify_failed",
            "the package header is not a backup package header",
        ));
    }
    let size = usize::try_from(length)
        .map_err(|_| TargetError::new("backup.verify_failed", "the package header is too long"))?;
    let mut text = vec![0u8; size];
    reader.read_exact(&mut text).map_err(header)?;
    let manifest = std::str::from_utf8(&text)
        .map_err(|_| TargetError::new("backup.verify_failed", "the package header is not UTF-8"))
        .and_then(|t| BackupManifest::from_toml(t).map_err(TargetError::from))?;
    if manifest.tenant != expected_tenant {
        return Err(TargetError::new(
            TARGET_FOREIGN,
            "the package belongs to another tenant and is refused",
        ));
    }
    let mut prefix = head.to_vec();
    prefix.extend_from_slice(&text);
    Ok(Box::new(Cursor::new(prefix).chain(reader)))
}

struct FolderAdapter {
    name: String,
    target: FolderTarget,
}

impl TargetAdapter for FolderAdapter {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "folder"
    }
    fn target(&self) -> &dyn BackupTarget {
        &self.target
    }
    fn preflight(&self) -> Outcome<()> {
        match self.target.status() {
            spot_backup::TargetStatus::Ready => Ok(()),
            spot_backup::TargetStatus::WaitingForMedia { volume } => Err(TargetError::new(
                TARGET_ABSENT,
                format!("the volume {volume} is not inserted"),
            )),
            spot_backup::TargetStatus::Unavailable { reason } => {
                Err(TargetError::new("backup.target_unavailable", reason))
            }
        }
    }
    fn fetch(&self, id: &str, expected_tenant: &str) -> Outcome<Box<dyn Read + Send>> {
        id_ok(id)?;
        tenant_checked(self.target.get(id)?, expected_tenant)
    }
}

struct RemovableAdapter {
    name: String,
    target: RemovableTarget,
    selected: Arc<SelectedVolume>,
    subdir: String,
}

impl TargetAdapter for RemovableAdapter {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "removable"
    }
    fn target(&self) -> &dyn BackupTarget {
        &self.target
    }
    fn preflight(&self) -> Outcome<()> {
        self.selected.assess().map(|_| ())
    }
    fn fetch(&self, id: &str, expected_tenant: &str) -> Outcome<Box<dyn Read + Send>> {
        id_ok(id)?;
        let found = self.selected.assess()?;
        let path = found
            .volume
            .mount_point
            .join(&self.subdir)
            .join(format!("{id}.{PACKAGE_EXTENSION}"));
        if !path.is_file() {
            return Err(TargetError::new(
                TARGET_NOT_FOUND,
                format!("no backup {id} exists on the volume"),
            ));
        }
        let package = spot_backup::package::open(&path)?;
        if package.manifest().tenant != expected_tenant {
            return Err(TargetError::new(
                TARGET_FOREIGN,
                "the package belongs to another tenant and is refused",
            ));
        }
        spot_backup::package::verify_quick(&package)?;
        Ok(package.reader()?)
    }
    fn eject_hint(&self) -> Option<String> {
        self.selected.assess().ok().map(|a| eject_hint(&a.device))
    }
}

struct CloudAdapter {
    name: String,
    target: CloudTarget,
    store: Arc<ChunkedCloudStore>,
}

impl TargetAdapter for CloudAdapter {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "cloud"
    }
    fn target(&self) -> &dyn BackupTarget {
        &self.target
    }
    fn preflight(&self) -> Outcome<()> {
        self.store
            .remaining_bytes()
            .map(|_| ())
            .map_err(TargetError::from)
    }
    fn fetch(&self, id: &str, expected_tenant: &str) -> Outcome<Box<dyn Read + Send>> {
        id_ok(id)?;
        tenant_checked(self.target.get(id)?, expected_tenant)
    }
}

// ---------------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------------

/// The port: named targets, opened on demand.
pub trait BackupTargets: Send + Sync {
    /// The configured target names, sorted.
    fn names(&self) -> Vec<String>;
    /// Opens target `name`.
    ///
    /// # Errors
    /// `backup.not_found` for an unknown name, `backup.target_invalid` for a bad configuration,
    /// `backup.unsupported` for a cloud target without a connector or a tenant key.
    fn open(&self, name: &str) -> Outcome<Arc<dyn TargetAdapter>>;
}

/// The cloud pieces the host supplies.
#[derive(Clone)]
pub struct CloudAccess {
    /// Reads the access secret.
    pub credentials: Arc<dyn CredentialSource>,
    /// Opens the S3-compatible store.
    pub connector: Arc<dyn CloudConnector>,
    /// The tenant's key; it never leaves the host.
    pub tenant_key: RecoveryKey,
}

/// The registry of the configured targets.
pub struct TargetRegistry {
    specs: BTreeMap<String, TargetSpec>,
    devices: Arc<dyn DeviceTable>,
    space: Arc<dyn SpaceProbe>,
    cloud: Option<CloudAccess>,
}

impl TargetRegistry {
    /// A registry of `specs` over `devices` (use [`SystemDevices`] on a real host).
    pub fn new(specs: BTreeMap<String, TargetSpec>, devices: Arc<dyn DeviceTable>) -> Self {
        Self {
            specs,
            devices,
            space: Arc::new(SystemSpaceProbe),
            cloud: None,
        }
    }

    /// The same registry with cloud targets enabled.
    #[must_use]
    pub fn with_cloud(mut self, cloud: CloudAccess) -> Self {
        self.cloud = Some(cloud);
        self
    }

    /// The same registry with another free-space probe, for tests.
    #[must_use]
    pub fn with_space(mut self, space: Arc<dyn SpaceProbe>) -> Self {
        self.space = space;
        self
    }
}

impl BackupTargets for TargetRegistry {
    fn names(&self) -> Vec<String> {
        self.specs.keys().cloned().collect()
    }

    fn open(&self, name: &str) -> Outcome<Arc<dyn TargetAdapter>> {
        let spec = self.specs.get(name).ok_or_else(|| {
            TargetError::new(TARGET_NOT_FOUND, format!("there is no target `{name}`"))
        })?;
        if let Some(first) = spec.problems().into_iter().next() {
            return Err(TargetError::new(
                TARGET_INVALID,
                format!("target `{name}`: {first}"),
            ));
        }
        let name = name.to_owned();
        match spec {
            TargetSpec::Folder { path } => Ok(Arc::new(FolderAdapter {
                name,
                target: FolderTarget::new(path.clone(), Arc::clone(&self.space)),
            })),
            TargetSpec::Removable { volume, subdir } => {
                let selected = Arc::new(SelectedVolume::new(
                    Arc::clone(&self.devices),
                    VolumeSelector::parse(volume)?,
                ));
                let subdir = subdir.clone().unwrap_or_else(|| DEFAULT_SUBDIR.to_owned());
                let probe: Arc<dyn VolumeProbe> = Arc::clone(&selected) as Arc<dyn VolumeProbe>;
                let target = RemovableTarget::new(
                    probe,
                    selected.selector().id(),
                    subdir.clone(),
                    None,
                    false,
                    None,
                );
                Ok(Arc::new(RemovableAdapter {
                    name,
                    target,
                    selected,
                    subdir,
                }))
            }
            TargetSpec::Cloud {
                endpoint,
                key_id,
                credential_ref,
                prefix,
                max_kbps,
                quota_bytes,
            } => {
                let cloud = self.cloud.as_ref().ok_or_else(|| {
                    TargetError::new(
                        TARGET_UNSUPPORTED,
                        "cloud targets need a connector and the tenant key; none is configured on this host",
                    )
                })?;
                let credential = cloud.credentials.credential(credential_ref)?;
                let objects = cloud.connector.connect(endpoint, key_id, &credential)?;
                let prefix = prefix.clone().unwrap_or_default();
                let store = Arc::new(ChunkedCloudStore::new(
                    objects,
                    cloud.tenant_key.clone(),
                    prefix.clone(),
                    *max_kbps,
                    *quota_bytes,
                ));
                let port: Arc<dyn CloudStore> = Arc::clone(&store) as Arc<dyn CloudStore>;
                Ok(Arc::new(CloudAdapter {
                    name,
                    target: CloudTarget::new(port, prefix),
                    store,
                }))
            }
        }
    }
}
