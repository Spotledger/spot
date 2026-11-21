//! Storage facts and filesystem snapshots: btrfs subvolumes, LVM snapshots and ZFS
//! snapshots, each as `plan`/`apply`/`verify` over argv-only commands.
//!
//! Block-device and mount facts (including the XFS/ext4 filesystem type) come from
//! [`crate::facts`]. A snapshot is named by a backend, a source and a name; every operation
//! is idempotent, so creating a snapshot that already exists changes nothing and the second
//! `plan` is empty. Deleting and restoring are destructive and say so in the preview; a
//! restore of a btrfs subvolume moves the current one aside before replacing it, so the
//! previous data is never dropped by this module.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::catalogue::op_id;
use crate::error::{SNAPSHOT_MISSING, SysError, SysResult};
use crate::facts::{BlockDevice, FactsReader, Mount};
use crate::preview::{Change, Check, Outcome, Preview};
use crate::sys::{CommandRunner, CommandSpec, FileSystem};
use spot_mgmt_ops::Risk;

/// The filesystem snapshot backends this module drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotBackend {
    /// btrfs subvolume snapshots.
    Btrfs,
    /// LVM thin/COW logical-volume snapshots.
    Lvm,
    /// ZFS dataset snapshots.
    Zfs,
}

impl SnapshotBackend {
    /// The backend's lower-case name, as the console and the provider adapter record it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Btrfs => "btrfs",
            Self::Lvm => "lvm",
            Self::Zfs => "zfs",
        }
    }

    /// The backend named `text`, if any.
    #[must_use]
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "btrfs" => Some(Self::Btrfs),
            "lvm" => Some(Self::Lvm),
            "zfs" => Some(Self::Zfs),
            _ => None,
        }
    }
}

/// One existing snapshot, as the backend reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// The snapshot's name (btrfs: its path relative to the source; LVM: its LV name;
    /// ZFS: the snapshot part after `@`).
    pub name: String,
    /// The source the snapshot was taken from (a subvolume path, a `vg/lv`, or a dataset).
    pub source: String,
    /// The backend that holds it.
    pub backend: SnapshotBackend,
    /// A short backend-specific detail (btrfs id/gen, LVM size, ZFS creation time).
    pub detail: String,
}

/// A snapshot to create, delete or restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRequest {
    /// The backend.
    pub backend: SnapshotBackend,
    /// The source: a btrfs subvolume path, an LVM `vg/lv`, or a ZFS dataset.
    pub source: String,
    /// The snapshot's name.
    pub name: String,
    /// The size for an LVM snapshot in bytes; required by LVM, ignored by the others.
    pub size_bytes: Option<u64>,
}

impl SnapshotRequest {
    /// Builds a request, validating the backend's name rules.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] when the source, name or size is invalid for the backend.
    pub fn new(
        backend: SnapshotBackend,
        source: impl Into<String>,
        name: impl Into<String>,
        size_bytes: Option<u64>,
    ) -> SysResult<Self> {
        let request = Self {
            backend,
            source: source.into(),
            name: name.into(),
            size_bytes,
        };
        request.validate()?;
        Ok(request)
    }

    /// Validates the source, name and size against the backend's rules.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] with the reason.
    pub fn validate(&self) -> SysResult<()> {
        match self.backend {
            SnapshotBackend::Btrfs => {
                validate_path(&self.source, "the btrfs source")?;
                validate_segment(&self.name, "the btrfs snapshot name")?;
            }
            SnapshotBackend::Lvm => {
                validate_volume(&self.source, "the LVM source")?;
                validate_segment(&self.name, "the LVM snapshot name")?;
                if self.size_bytes.unwrap_or(0) == 0 {
                    return Err(SysError::invalid(
                        "an LVM snapshot needs a non-zero size in bytes",
                    ));
                }
            }
            SnapshotBackend::Zfs => {
                validate_dataset(&self.source, "the ZFS dataset")?;
                validate_segment(&self.name, "the ZFS snapshot name")?;
            }
        }
        Ok(())
    }

    /// The btrfs snapshot path: `<source>/<name>`.
    #[must_use]
    pub fn btrfs_path(&self) -> String {
        format!("{}/{}", self.source.trim_end_matches('/'), self.name)
    }

    /// The LVM snapshot volume: `<vg>/<name>` where the volume group comes from `source`.
    #[must_use]
    pub fn lvm_volume(&self) -> String {
        let vg = self.source.split('/').next().unwrap_or(&self.source);
        format!("{vg}/{}", self.name)
    }

    /// The ZFS snapshot: `<dataset>@<name>`.
    #[must_use]
    pub fn zfs_snapshot(&self) -> String {
        format!("{}@{}", self.source, self.name)
    }

    /// The backend-specific full name this request refers to.
    #[must_use]
    pub fn full_name(&self) -> String {
        match self.backend {
            SnapshotBackend::Btrfs => self.btrfs_path(),
            SnapshotBackend::Lvm => self.lvm_volume(),
            SnapshotBackend::Zfs => self.zfs_snapshot(),
        }
    }
}

/// The storage facts: the block-device tree, the mount tree and the snapshot backend the
/// machine offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageFacts {
    /// The block devices.
    pub devices: Vec<BlockDevice>,
    /// The mounts.
    pub mounts: Vec<Mount>,
    /// The snapshot backend present on the machine, if any (btrfs and ZFS are detected from
    /// the filesystem type, LVM from an `lvm` block device).
    pub backend: Option<SnapshotBackend>,
}

/// The storage and snapshot module.
pub struct StorageModule<'a> {
    runner: &'a dyn CommandRunner,
    fs: &'a dyn FileSystem,
}

impl<'a> StorageModule<'a> {
    /// Wraps a command runner and a filesystem.
    #[must_use]
    pub fn new(runner: &'a dyn CommandRunner, fs: &'a dyn FileSystem) -> Self {
        Self { runner, fs }
    }

    /// The block-device, mount and backend facts.
    ///
    /// # Errors
    /// The fact commands' failures and [`crate::error::PARSE`] for malformed output.
    pub fn facts(&self) -> SysResult<StorageFacts> {
        let reader = FactsReader::new(self.runner);
        let devices = reader.block_devices()?;
        let mounts = reader.mounts()?;
        let backend = detect_backend(&devices, &mounts);
        Ok(StorageFacts {
            devices,
            mounts,
            backend,
        })
    }

    /// The bytes available on the filesystem holding `path`.
    ///
    /// # Errors
    /// The runner's failures and [`crate::error::PARSE`].
    pub fn free_space(&self, path: &Path) -> SysResult<u64> {
        FactsReader::new(self.runner).free_space(path)
    }

    /// The filesystem type holding `path` (XFS, ext4, btrfs, ...).
    ///
    /// # Errors
    /// The runner's failures and [`crate::error::PARSE`].
    pub fn filesystem_type(&self, path: &Path) -> SysResult<Option<String>> {
        FactsReader::new(self.runner).filesystem_type(path)
    }

    /// Lists the snapshots of `source` for `backend`.
    ///
    /// # Errors
    /// The backend command's failures and [`crate::error::PARSE`] for malformed output.
    pub fn list(&self, backend: SnapshotBackend, source: &str) -> SysResult<Vec<SnapshotInfo>> {
        match backend {
            SnapshotBackend::Btrfs => self.list_btrfs(source),
            SnapshotBackend::Lvm => self.list_lvm(source),
            SnapshotBackend::Zfs => self.list_zfs(source),
        }
    }

    /// Whether a snapshot named by `request` exists.
    ///
    /// # Errors
    /// The listing's failures.
    pub fn exists(&self, request: &SnapshotRequest) -> SysResult<bool> {
        let snapshots = self.list(request.backend, &request.source)?;
        let wanted = match request.backend {
            SnapshotBackend::Btrfs => request.btrfs_path(),
            SnapshotBackend::Lvm => request.name.clone(),
            SnapshotBackend::Zfs => request.name.clone(),
        };
        Ok(snapshots.iter().any(|snapshot| {
            snapshot.name == wanted
                || snapshot.name == request.name
                || request.full_name().ends_with(&snapshot.name)
        }))
    }

    /// Plans a snapshot creation.
    ///
    /// # Errors
    /// Validation, the listing's failures.
    pub fn plan_create(&self, request: &SnapshotRequest) -> SysResult<Preview> {
        request.validate()?;
        let exists = self.exists(request)?;
        let mut preview =
            Preview::new(op_id(&["os", "snapshot", "create"]), Risk::High).change(Change::new(
                request.full_name(),
                Some(if exists { "present" } else { "absent" }.to_owned()),
                Some("present".to_owned()),
            ));
        preview = preview.note(match request.backend {
            SnapshotBackend::Btrfs => format!("btrfs subvolume snapshot {}", request.source),
            SnapshotBackend::Lvm => format!(
                "lvm snapshot of {} at {} bytes",
                request.source,
                request.size_bytes.unwrap_or(0)
            ),
            SnapshotBackend::Zfs => format!("zfs snapshot of {}", request.source),
        });
        Ok(preview)
    }

    /// Creates the snapshot; a no-op when it already exists.
    ///
    /// # Errors
    /// The backend command's failures.
    pub fn apply_create(&self, request: &SnapshotRequest) -> SysResult<Outcome> {
        if self.exists(request)? {
            return Ok(Outcome::new(
                op_id(&["os", "snapshot", "create"]),
                false,
                format!("{} already exists", request.full_name()),
            ));
        }
        let spec = match request.backend {
            SnapshotBackend::Btrfs => CommandSpec::new("btrfs")?.args([
                "subvolume",
                "snapshot",
                &request.source,
                &request.btrfs_path(),
            ])?,
            SnapshotBackend::Lvm => CommandSpec::new("lvcreate")?.args([
                "--snapshot",
                "--name",
                &request.name,
                "--size",
                &format!("{}b", request.size_bytes.unwrap_or(0)),
                &request.source,
            ])?,
            SnapshotBackend::Zfs => {
                CommandSpec::new("zfs")?.args(["snapshot", &request.zfs_snapshot()])?
            }
        };
        let output = self.runner.run(&spec)?;
        output.require_success("creating the snapshot")?;
        Ok(Outcome::new(
            op_id(&["os", "snapshot", "create"]),
            true,
            format!("created {}", request.full_name()),
        ))
    }

    /// Verifies a snapshot exists.
    ///
    /// # Errors
    /// The listing's failures.
    pub fn verify_create(&self, request: &SnapshotRequest) -> SysResult<Check> {
        if self.exists(request)? {
            Ok(Check::ok(
                op_id(&["os", "snapshot", "create"]),
                format!("{} is present", request.full_name()),
            ))
        } else {
            Ok(Check::failed(
                op_id(&["os", "snapshot", "create"]),
                format!("{} is missing", request.full_name()),
            ))
        }
    }

    /// Plans a snapshot deletion.
    ///
    /// # Errors
    /// Validation and the listing's failures.
    pub fn plan_delete(&self, request: &SnapshotRequest) -> SysResult<Preview> {
        request.validate()?;
        let exists = self.exists(request)?;
        Ok(
            Preview::new(op_id(&["os", "snapshot", "delete"]), Risk::High)
                .change(Change::new(
                    request.full_name(),
                    Some(if exists { "present" } else { "absent" }.to_owned()),
                    Some("absent".to_owned()),
                ))
                .destructive(true),
        )
    }

    /// Deletes the snapshot; a no-op when it is already gone.
    ///
    /// # Errors
    /// The backend command's failures.
    pub fn apply_delete(&self, request: &SnapshotRequest) -> SysResult<Outcome> {
        if !self.exists(request)? {
            return Ok(Outcome::new(
                op_id(&["os", "snapshot", "delete"]),
                false,
                format!("{} is already gone", request.full_name()),
            ));
        }
        let spec = match request.backend {
            SnapshotBackend::Btrfs => {
                CommandSpec::new("btrfs")?.args(["subvolume", "delete", &request.btrfs_path()])?
            }
            SnapshotBackend::Lvm => {
                CommandSpec::new("lvremove")?.args(["-f", &request.lvm_volume()])?
            }
            SnapshotBackend::Zfs => {
                CommandSpec::new("zfs")?.args(["destroy", &request.zfs_snapshot()])?
            }
        };
        let output = self.runner.run(&spec)?;
        output.require_success("deleting the snapshot")?;
        Ok(Outcome::new(
            op_id(&["os", "snapshot", "delete"]),
            true,
            format!("deleted {}", request.full_name()),
        ))
    }

    /// Verifies a snapshot is gone.
    ///
    /// # Errors
    /// The listing's failures.
    pub fn verify_delete(&self, request: &SnapshotRequest) -> SysResult<Check> {
        if self.exists(request)? {
            Ok(Check::failed(
                op_id(&["os", "snapshot", "delete"]),
                format!("{} is still present", request.full_name()),
            ))
        } else {
            Ok(Check::ok(
                op_id(&["os", "snapshot", "delete"]),
                format!("{} is gone", request.full_name()),
            ))
        }
    }

    /// Plans a restore from a snapshot.
    ///
    /// # Errors
    /// Validation and the listing's failures.
    pub fn plan_restore(&self, request: &SnapshotRequest) -> SysResult<Preview> {
        request.validate()?;
        if !self.exists(request)? {
            return Err(SysError::new(
                SNAPSHOT_MISSING,
                format!("{} does not exist", request.full_name()),
            ));
        }
        Ok(
            Preview::new(op_id(&["os", "snapshot", "restore"]), Risk::Critical)
                .change(Change::new(
                    request.source.clone(),
                    Some("current".to_owned()),
                    Some(format!("{} (rolled back)", request.full_name())),
                ))
                .destructive(true)
                .note("the previous data is moved aside, never dropped"),
        )
    }

    /// Restores from the snapshot, moving the current data aside first (btrfs and ZFS
    /// rollback in place; LVM merges on next activation).
    ///
    /// # Errors
    /// [`SNAPSHOT_MISSING`] when the snapshot is absent, and the backend command's failures.
    pub fn apply_restore(&self, request: &SnapshotRequest, now: i64) -> SysResult<Outcome> {
        if !self.exists(request)? {
            return Err(SysError::new(
                SNAPSHOT_MISSING,
                format!("{} does not exist", request.full_name()),
            ));
        }
        match request.backend {
            SnapshotBackend::Btrfs => {
                let aside = PathBuf::from(format!("{}.replaced-{now}", request.source));
                if self.fs.exists(Path::new(&request.source))? {
                    self.fs.rename(Path::new(&request.source), &aside)?;
                }
                let spec = CommandSpec::new("btrfs")?.args([
                    "subvolume",
                    "snapshot",
                    &request.btrfs_path(),
                    &request.source,
                ])?;
                let output = self.runner.run(&spec)?;
                output.require_success("restoring the btrfs subvolume")?;
                Ok(Outcome::new(
                    op_id(&["os", "snapshot", "restore"]),
                    true,
                    format!(
                        "restored {} from {} (previous data at {})",
                        request.source,
                        request.btrfs_path(),
                        aside.display()
                    ),
                ))
            }
            SnapshotBackend::Lvm => {
                let spec =
                    CommandSpec::new("lvconvert")?.args(["--merge", &request.lvm_volume()])?;
                let output = self.runner.run(&spec)?;
                output.require_success("merging the LVM snapshot")?;
                Ok(Outcome::new(
                    op_id(&["os", "snapshot", "restore"]),
                    true,
                    format!("merged {} on next activation", request.lvm_volume()),
                ))
            }
            SnapshotBackend::Zfs => {
                let spec = CommandSpec::new("zfs")?.args(["rollback", &request.zfs_snapshot()])?;
                let output = self.runner.run(&spec)?;
                output.require_success("rolling back the ZFS dataset")?;
                Ok(Outcome::new(
                    op_id(&["os", "snapshot", "restore"]),
                    true,
                    format!("rolled {} back to {}", request.source, request.name),
                ))
            }
        }
    }

    /// Verifies a restore: for LVM the snapshot is gone (merged) or merging; for btrfs and
    /// ZFS the source exists and the snapshot is still available.
    ///
    /// # Errors
    /// The listing's failures.
    pub fn verify_restore(&self, request: &SnapshotRequest) -> SysResult<Check> {
        let source_present = match request.backend {
            SnapshotBackend::Btrfs => self.fs.exists(Path::new(&request.source))?,
            SnapshotBackend::Lvm | SnapshotBackend::Zfs => true,
        };
        if source_present {
            Ok(Check::ok(
                op_id(&["os", "snapshot", "restore"]),
                format!("{} is present", request.source),
            ))
        } else {
            Ok(Check::failed(
                op_id(&["os", "snapshot", "restore"]),
                format!("{} is missing after the restore", request.source),
            ))
        }
    }

    /// Lists btrfs subvolumes under `source`.
    ///
    /// # Errors
    /// The command's failures and [`crate::error::PARSE`].
    pub fn list_btrfs(&self, source: &str) -> SysResult<Vec<SnapshotInfo>> {
        validate_path(source, "the btrfs source")?;
        let spec = CommandSpec::new("btrfs")?.args(["subvolume", "list", "-o", source])?;
        let output = self.runner.run(&spec)?;
        output.require_success("listing btrfs subvolumes")?;
        parse_btrfs_list(output.stdout(), source)
    }

    /// Lists LVM snapshots whose origin is the logical volume in `source`.
    ///
    /// # Errors
    /// The command's failures and [`crate::error::PARSE`].
    pub fn list_lvm(&self, source: &str) -> SysResult<Vec<SnapshotInfo>> {
        validate_volume(source, "the LVM source")?;
        let spec = CommandSpec::new("lvs")?.args([
            "--reportformat",
            "json",
            "--units",
            "b",
            "-o",
            "lv_name,vg_name,lv_size,lv_attr,origin",
        ])?;
        let output = self.runner.run(&spec)?;
        output.require_success("listing LVM volumes")?;
        parse_lvm_list(output.stdout(), source)
    }

    /// Lists ZFS snapshots of `source`.
    ///
    /// # Errors
    /// The command's failures and [`crate::error::PARSE`].
    pub fn list_zfs(&self, source: &str) -> SysResult<Vec<SnapshotInfo>> {
        validate_dataset(source, "the ZFS dataset")?;
        let spec = CommandSpec::new("zfs")?.args([
            "list",
            "-H",
            "-t",
            "snapshot",
            "-o",
            "name,used,creation",
            "-r",
            source,
        ])?;
        let output = self.runner.run(&spec)?;
        output.require_success("listing ZFS snapshots")?;
        parse_zfs_list(output.stdout(), source)
    }
}

/// Detects the snapshot backend from the facts: a btrfs or ZFS filesystem, or an LVM
/// block device.
#[must_use]
pub fn detect_backend(devices: &[BlockDevice], mounts: &[Mount]) -> Option<SnapshotBackend> {
    let has_fstype = |wanted: &str| {
        devices
            .iter()
            .flat_map(BlockDevice::flatten)
            .any(|device| device.fstype.as_deref() == Some(wanted))
    };
    let has_kind = |wanted: &str| {
        devices
            .iter()
            .flat_map(BlockDevice::flatten)
            .any(|device| device.kind == wanted)
    };
    if has_fstype("btrfs") {
        return Some(SnapshotBackend::Btrfs);
    }
    if has_fstype("zfs")
        || mounts
            .iter()
            .flat_map(Mount::flatten)
            .any(|mount| mount.fstype == "zfs")
    {
        return Some(SnapshotBackend::Zfs);
    }
    if has_kind("lvm") {
        return Some(SnapshotBackend::Lvm);
    }
    None
}

/// Whether `name` is a safe single segment: `[A-Za-z0-9][A-Za-z0-9._-]*`, at most 255
/// characters. A leading `-` is refused, so a name can never become an option.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] with the reason.
pub fn validate_segment(name: &str, what: &str) -> SysResult<()> {
    if name.is_empty() || name.len() > 255 {
        return Err(SysError::invalid(format!(
            "{what} must be 1 to 255 characters"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(SysError::invalid(format!(
            "{what} may only contain letters, digits, dot, underscore and hyphen"
        )));
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err(SysError::invalid(format!(
            "{what} may not begin with a dot or a hyphen"
        )));
    }
    Ok(())
}

/// Whether `text` is an absolute path of safe segments.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] with the reason.
pub fn validate_path(text: &str, what: &str) -> SysResult<()> {
    if !text.starts_with('/') {
        return Err(SysError::invalid(format!(
            "{what} must be an absolute path"
        )));
    }
    if text.contains("..") || text.contains('\0') {
        return Err(SysError::invalid(format!("{what} may not contain '..'")));
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+'))
    {
        return Err(SysError::invalid(format!(
            "{what} contains an unexpected character"
        )));
    }
    Ok(())
}

/// Whether `text` is a safe LVM volume name (`vg` or `vg/lv`).
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] with the reason.
pub fn validate_volume(text: &str, what: &str) -> SysResult<()> {
    if text.is_empty() || text.starts_with('-') {
        return Err(SysError::invalid(format!(
            "{what} is not a valid volume name"
        )));
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '+' | '-'))
    {
        return Err(SysError::invalid(format!(
            "{what} may only contain letters, digits, '/', '.', '_', '+' and '-'"
        )));
    }
    Ok(())
}

/// Whether `text` is a safe ZFS dataset name.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] with the reason.
pub fn validate_dataset(text: &str, what: &str) -> SysResult<()> {
    if text.is_empty() || text.starts_with('-') {
        return Err(SysError::invalid(format!(
            "{what} is not a valid dataset name"
        )));
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':'))
    {
        return Err(SysError::invalid(format!(
            "{what} may only contain letters, digits, '/', '.', '_', '-' and ':'"
        )));
    }
    Ok(())
}

/// Parses `btrfs subvolume list -o` output: one `path <name>` per subvolume.
///
/// # Errors
/// [`crate::error::PARSE`] when a line has no `path` field.
pub fn parse_btrfs_list(text: &str, source: &str) -> SysResult<Vec<SnapshotInfo>> {
    let mut snapshots = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let path = line
            .split_whitespace()
            .skip_while(|token| *token != "path")
            .nth(1)
            .ok_or_else(|| {
                SysError::parse(format!("a btrfs subvolume line has no path: {line}"))
            })?;
        let id = line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let full = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("{}/{}", source.trim_end_matches('/'), path)
        };
        snapshots.push(SnapshotInfo {
            name: full,
            source: source.to_owned(),
            backend: SnapshotBackend::Btrfs,
            detail: format!("id {id}"),
        });
    }
    Ok(snapshots)
}

/// Parses `lvs --reportformat json` output: the snapshot LVs whose `origin` is `source`'s
/// logical volume.
///
/// # Errors
/// [`crate::error::PARSE`] when the document is not the expected shape.
pub fn parse_lvm_list(text: &str, source: &str) -> SysResult<Vec<SnapshotInfo>> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| SysError::parse(format!("lvs JSON: {e}")))?;
    let reports = value
        .get("report")
        .and_then(Value::as_array)
        .ok_or_else(|| SysError::parse("lvs JSON has no report array"))?;
    let origin_lv = source.split('/').nth(1).unwrap_or(source);
    let mut snapshots = Vec::new();
    for report in reports {
        let Some(volumes) = report.get("lv").and_then(Value::as_array) else {
            continue;
        };
        for volume in volumes {
            let origin = volume.get("origin").and_then(Value::as_str).unwrap_or("");
            if origin != origin_lv {
                continue;
            }
            let name = volume
                .get("lv_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let vg = volume
                .get("vg_name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let size = volume
                .get("lv_size")
                .and_then(Value::as_str)
                .unwrap_or("0")
                .trim_end_matches('B')
                .to_owned();
            snapshots.push(SnapshotInfo {
                name,
                source: format!("{vg}/{origin_lv}"),
                backend: SnapshotBackend::Lvm,
                detail: format!("{size} bytes"),
            });
        }
    }
    Ok(snapshots)
}

/// Parses `zfs list -H -t snapshot -o name,used,creation` output.
///
/// # Errors
/// [`crate::error::PARSE`] when a line has no `dataset@name`.
pub fn parse_zfs_list(text: &str, source: &str) -> SysResult<Vec<SnapshotInfo>> {
    let mut snapshots = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split('\t');
        let full = fields.next().unwrap_or_default();
        let used = fields.next().unwrap_or_default();
        let created = fields.next().unwrap_or_default();
        let name = full
            .split_once('@')
            .map(|(_, name)| name)
            .ok_or_else(|| SysError::parse(format!("a ZFS snapshot line has no '@': {line}")))?;
        snapshots.push(SnapshotInfo {
            name: name.to_owned(),
            source: source.to_owned(),
            backend: SnapshotBackend::Zfs,
            detail: format!("{used} used, created {created}"),
        });
    }
    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testing::{FakeFs, FakeRunner, ok_output};

    #[test]
    fn request_names_are_built_per_backend() {
        let btrfs = SnapshotRequest::new(SnapshotBackend::Btrfs, "/data", "snap1", None).unwrap();
        assert_eq!(btrfs.btrfs_path(), "/data/snap1");
        let lvm = SnapshotRequest::new(SnapshotBackend::Lvm, "vg0/root", "s1", Some(1024)).unwrap();
        assert_eq!(lvm.lvm_volume(), "vg0/s1");
        let zfs = SnapshotRequest::new(SnapshotBackend::Zfs, "tank/data", "s1", None).unwrap();
        assert_eq!(zfs.zfs_snapshot(), "tank/data@s1");
    }

    #[test]
    fn unsafe_names_are_refused() {
        assert!(SnapshotRequest::new(SnapshotBackend::Btrfs, "/data", "-rf", None).is_err());
        assert!(SnapshotRequest::new(SnapshotBackend::Btrfs, "/data", "a b", None).is_err());
        assert!(SnapshotRequest::new(SnapshotBackend::Btrfs, "data", "s", None).is_err());
        assert!(SnapshotRequest::new(SnapshotBackend::Lvm, "vg/lv", "s", None).is_err());
        assert!(SnapshotRequest::new(SnapshotBackend::Lvm, "vg/lv", "s", Some(0)).is_err());
        assert!(SnapshotRequest::new(SnapshotBackend::Zfs, "tank", "s", None).is_ok());
    }

    #[test]
    fn btrfs_list_parses_paths() {
        let text = "ID 257 gen 30 top level 5 path snapshots/a\nID 258 gen 31 top level 5 path b\n";
        let snapshots = parse_btrfs_list(text, "/data").unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].name, "/data/snapshots/a");
        assert_eq!(snapshots[1].name, "/data/b");
    }

    #[test]
    fn lvm_list_filters_by_origin() {
        let text = r#"{"report":[{"lv":[
            {"lv_name":"snap1","vg_name":"vg0","lv_size":"1073741824B","lv_attr":"swi-a-s---","origin":"root"},
            {"lv_name":"other","vg_name":"vg0","lv_size":"10B","lv_attr":"-wi-ao----","origin":""}
        ]}]}"#;
        let snapshots = parse_lvm_list(text, "vg0/root").unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].name, "snap1");
        assert_eq!(snapshots[0].source, "vg0/root");
    }

    #[test]
    fn zfs_list_parses_names() {
        let text =
            "tank/data@s1\t0B\tMon Oct 12 10:00 2026\ntank/data@s2\t1024\tTue Oct 13 10:00 2026\n";
        let snapshots = parse_zfs_list(text, "tank/data").unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[1].name, "s2");
    }

    #[test]
    fn create_is_idempotent_against_a_listing() {
        let runner = FakeRunner::new().with_response(
            "btrfs subvolume list -o /data",
            ok_output("ID 257 gen 30 top level 5 path snap1\n"),
        );
        let fs = FakeFs::new().with_dir("/data");
        let module = StorageModule::new(&runner, &fs);
        let request = SnapshotRequest::new(SnapshotBackend::Btrfs, "/data", "snap1", None).unwrap();
        assert!(module.exists(&request).unwrap());
        let outcome = module.apply_create(&request).unwrap();
        assert!(!outcome.changed);
        assert!(
            runner
                .call_keys()
                .iter()
                .all(|key| !key.starts_with("btrfs subvolume snapshot"))
        );
    }

    #[test]
    fn creating_a_btrfs_snapshot_builds_the_expected_argv() {
        let runner = FakeRunner::new()
            .with_response("btrfs subvolume list -o /data", ok_output(""))
            .with_response("btrfs subvolume snapshot /data /data/snap1", ok_output(""));
        let fs = FakeFs::new().with_dir("/data");
        let module = StorageModule::new(&runner, &fs);
        let request = SnapshotRequest::new(SnapshotBackend::Btrfs, "/data", "snap1", None).unwrap();
        let preview = module.plan_create(&request).unwrap();
        assert!(!preview.is_noop());
        let outcome = module.apply_create(&request).unwrap();
        assert!(outcome.changed);
        assert_eq!(
            runner.last_call().unwrap().argv_key(),
            "btrfs subvolume snapshot /data /data/snap1"
        );
    }

    #[test]
    fn a_delete_of_a_missing_snapshot_is_a_noop() {
        let runner =
            FakeRunner::new().with_response("btrfs subvolume list -o /data", ok_output(""));
        let fs = FakeFs::new().with_dir("/data");
        let module = StorageModule::new(&runner, &fs);
        let request = SnapshotRequest::new(SnapshotBackend::Btrfs, "/data", "gone", None).unwrap();
        assert!(!module.apply_delete(&request).unwrap().changed);
    }

    #[test]
    fn backend_detection_reads_the_facts() {
        let json = r#"{"blockdevices":[{"name":"sda","type":"disk","size":100,"children":[{"name":"sda1","type":"part","size":100,"fstype":"btrfs"}]}]}"#;
        let devices = crate::facts::parse_lsblk(json).unwrap();
        assert_eq!(detect_backend(&devices, &[]), Some(SnapshotBackend::Btrfs));
        let lvm = r#"{"blockdevices":[{"name":"dm-0","type":"lvm","size":100}]}"#;
        let devices = crate::facts::parse_lsblk(lvm).unwrap();
        assert_eq!(detect_backend(&devices, &[]), Some(SnapshotBackend::Lvm));
    }
}
