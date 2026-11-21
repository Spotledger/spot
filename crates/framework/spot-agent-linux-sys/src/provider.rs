//! The snapshot adapter: a btrfs, LVM or ZFS filesystem snapshot exposed through the
//! `spot-snapshot` provider contract, so the migration layer (MIG-05) can use the Linux
//! backends through the same [`SnapshotProvider`] trait as the portable providers.
//!
//! A filesystem snapshot is not a copied directory, so the manifest records the platform
//! handle in `platform_extra` (`linux_backend`, `linux_source`, `linux_snapshot`) and the
//! `files` inventory stays empty; `verify` re-checks that the platform snapshot still
//! exists, and `delete` destroys it. The provider contract's target names a PostgreSQL
//! instance (`SnapshotTarget::Instance`), whose data directory is what the filesystem
//! snapshot covers; a database target is refused, because a filesystem snapshot needs a
//! path, not a connection string.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_core::Clock;
use spot_mgmt_ops::Risk;
use spot_snapshot::manifest::{DestinationPath, check_destination};
use spot_snapshot::{
    Capabilities, Consistency, Destination, Progress, ProgressSink, RestoreOptions, RestoreTarget,
    Snapshot, SnapshotError, SnapshotManifest, SnapshotOptions, SnapshotProvider, SnapshotResult,
    SnapshotTarget, TakeOutcome, VerifyReport,
};

use crate::catalogue::op_id;
use crate::error::{SNAPSHOT_MISSING, SNAPSHOT_TARGET, SysError, SysResult};
use crate::facts::{FactsReader, Mount};
use crate::preview::Preview;
use crate::storage::{SnapshotBackend, StorageModule};
use crate::sys::{CommandRunner, CommandSpec, FileSystem};

/// The manifest keys the adapter records.
pub const PLATFORM_BACKEND: &str = "linux_backend";
/// The source (subvolume path, `vg/lv` or dataset) the snapshot covers.
pub const PLATFORM_SOURCE: &str = "linux_source";
/// The platform handle to restore or destroy (a path, a `vg/lv` or a `dataset@name`).
pub const PLATFORM_SNAPSHOT: &str = "linux_snapshot";

/// The filesystem-snapshot provider: one backend over the command and filesystem seams.
pub struct LinuxSnapshotProvider<'a> {
    backend: SnapshotBackend,
    runner: &'a dyn CommandRunner,
    fs: &'a dyn FileSystem,
    clock: Arc<dyn Clock>,
    lvm_size_bytes: u64,
    counter: AtomicU64,
}

impl<'a> LinuxSnapshotProvider<'a> {
    /// Builds a provider for `backend`.
    #[must_use]
    pub fn new(
        backend: SnapshotBackend,
        runner: &'a dyn CommandRunner,
        fs: &'a dyn FileSystem,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            backend,
            runner,
            fs,
            clock,
            lvm_size_bytes: 0,
            counter: AtomicU64::new(0),
        }
    }

    /// Sets the LVM snapshot size in bytes (required by the LVM backend).
    #[must_use]
    pub fn with_lvm_size(mut self, bytes: u64) -> Self {
        self.lvm_size_bytes = bytes;
        self
    }

    /// The backend this provider drives.
    #[must_use]
    pub fn backend(&self) -> SnapshotBackend {
        self.backend
    }

    /// The provider kind the manifest records.
    #[must_use]
    pub fn provider_kind(&self) -> &'static str {
        self.backend.name()
    }

    /// The filesystem path a provider target names, or [`SNAPSHOT_TARGET`] for a database
    /// target the adapter cannot snapshot.
    ///
    /// # Errors
    /// [`SNAPSHOT_TARGET`] for a database target.
    pub fn target_path(target: &SnapshotTarget) -> SysResult<PathBuf> {
        match target {
            SnapshotTarget::Instance { layout, .. } => Ok(layout.data_dir.clone()),
            SnapshotTarget::Database { .. } => Err(SysError::new(
                SNAPSHOT_TARGET,
                "a filesystem snapshot needs a path; a database target has none",
            )),
        }
    }

    /// Discovers the backend source that holds `path`: the path itself for btrfs, the
    /// mounted dataset for ZFS, and the mounted `vg/lv` for LVM.
    ///
    /// # Errors
    /// The mount facts' failures and [`crate::error::UNSUPPORTED`] when no mount covers `path`.
    pub fn discover_source(&self, path: &Path) -> SysResult<String> {
        match self.backend {
            SnapshotBackend::Btrfs => Ok(path.display().to_string()),
            SnapshotBackend::Zfs | SnapshotBackend::Lvm => {
                let mounts = FactsReader::new(self.runner).mounts()?;
                let mount = deepest_mount(&mounts, path).ok_or_else(|| {
                    SysError::unsupported(format!(
                        "no mount covers {}; cannot find the {} source",
                        path.display(),
                        self.backend.name()
                    ))
                })?;
                match self.backend {
                    SnapshotBackend::Zfs => Ok(mount.source.clone()),
                    SnapshotBackend::Lvm => lvm_from_device(&mount.source).ok_or_else(|| {
                        SysError::unsupported(format!("{} is not an LVM device", mount.source))
                    }),
                    SnapshotBackend::Btrfs => Ok(path.display().to_string()),
                }
            }
        }
    }

    /// Plans a take, for the console: it resolves the target and the source and shows the
    /// platform handle that would be created.
    ///
    /// # Errors
    /// [`SNAPSHOT_TARGET`] and the discovery failures.
    pub fn plan_take(&self, target: &SnapshotTarget, dest: &Destination) -> SysResult<Preview> {
        let path = Self::target_path(target)?;
        let source = self.discover_source(&path)?;
        Ok(
            Preview::new(op_id(&["os", "snapshot", "create"]), Risk::High)
                .note(format!("{} snapshot of {source}", self.backend.name()))
                .note(format!("recorded under {}", dest.dir.display())),
        )
    }

    /// The provider's `take`, without the boxed future.
    async fn take_inner(
        &self,
        target: &SnapshotTarget,
        dest: &Destination,
        _opts: &SnapshotOptions,
        progress: Arc<dyn ProgressSink>,
    ) -> SnapshotResult<TakeOutcome> {
        let path = Self::target_path(target).map_err(to_snapshot_error)?;
        let source = self.discover_source(&path).map_err(to_snapshot_error)?;
        check_destination(&DestinationPath(dest.dir.clone())).await?;
        progress.on(Progress::Started {
            phase: 1,
            files_total: 0,
        });
        let id = self.next_id();
        let handle = self
            .create_platform_snapshot(&source, &dest.dir, &id)
            .map_err(to_snapshot_error)?;
        let manifest = self.manifest(target, &source, &handle);
        let published = manifest.publish(&dest.dir).await;
        let result = match published {
            Ok(()) => Snapshot::open(dest.dir.clone()).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(snapshot) => {
                progress.on(Progress::Finished { ok: true });
                Ok(TakeOutcome { snapshot })
            }
            Err(error) => {
                progress.on(Progress::Finished { ok: false });
                Err(error)
            }
        }
    }

    /// Runs the backend command that creates the platform snapshot and returns its handle.
    fn create_platform_snapshot(&self, source: &str, dest: &Path, id: &str) -> SysResult<String> {
        match self.backend {
            SnapshotBackend::Btrfs => {
                let destination = dest.display().to_string();
                let spec = CommandSpec::new("btrfs")?.args([
                    "subvolume",
                    "snapshot",
                    source,
                    &destination,
                ])?;
                let output = self.runner.run(&spec)?;
                output.require_success("creating the btrfs snapshot")?;
                Ok(destination)
            }
            SnapshotBackend::Lvm => {
                let name = format!("spot-{}", short_id(id));
                let volume = format!("{}/{}", source.split('/').next().unwrap_or(source), name);
                let spec = CommandSpec::new("lvcreate")?.args([
                    "--snapshot",
                    "--name",
                    &name,
                    "--size",
                    &format!("{}b", self.lvm_size_bytes),
                    source,
                ])?;
                let output = self.runner.run(&spec)?;
                output.require_success("creating the LVM snapshot")?;
                Ok(volume)
            }
            SnapshotBackend::Zfs => {
                let handle = format!("{}@spot-{}", source, short_id(id));
                let spec = CommandSpec::new("zfs")?.args(["snapshot", &handle])?;
                let output = self.runner.run(&spec)?;
                output.require_success("creating the ZFS snapshot")?;
                Ok(handle)
            }
        }
    }

    /// Builds the canonical manifest for a taken snapshot.
    fn manifest(&self, target: &SnapshotTarget, source: &str, handle: &str) -> SnapshotManifest {
        let mut platform_extra = std::collections::BTreeMap::new();
        platform_extra.insert(PLATFORM_BACKEND.to_owned(), self.backend.name().to_owned());
        platform_extra.insert(PLATFORM_SOURCE.to_owned(), source.to_owned());
        platform_extra.insert(PLATFORM_SNAPSHOT.to_owned(), handle.to_owned());
        SnapshotManifest {
            id: self.next_id(),
            tenant: target.tenant().to_owned(),
            provider: crate::CRATE_NAME.to_owned(),
            provider_version: env!("CARGO_PKG_VERSION").to_owned(),
            provider_kind: self.provider_kind().to_owned(),
            pg_major: spot_snapshot::SUPPORTED_MAJOR_VERSION,
            catalogue_version: String::new(),
            feed_schema: String::new(),
            outbox_generation: 0,
            outbox_last_published_seq: 0,
            started_at: SnapshotManifest::format_instant(self.clock.now_utc()),
            finished_at: SnapshotManifest::format_instant(self.clock.now_utc()),
            consistency: Consistency::Offline,
            total_bytes: 0,
            file_count: 0,
            files: std::collections::BTreeMap::new(),
            node_local: std::collections::BTreeMap::new(),
            platform_extra,
        }
    }

    /// The platform handle recorded in a snapshot's manifest.
    fn handle_of(&self, snapshot: &Snapshot) -> SnapshotResult<String> {
        snapshot
            .manifest()
            .platform_extra
            .get(PLATFORM_SNAPSHOT)
            .cloned()
            .ok_or_else(|| SnapshotError::VerifyFailed {
                reason: "the manifest records no linux_snapshot handle".to_owned(),
            })
    }

    /// The source recorded in a snapshot's manifest.
    fn source_of(&self, snapshot: &Snapshot) -> SnapshotResult<String> {
        snapshot
            .manifest()
            .platform_extra
            .get(PLATFORM_SOURCE)
            .cloned()
            .ok_or_else(|| SnapshotError::VerifyFailed {
                reason: "the manifest records no linux_source".to_owned(),
            })
    }

    /// Whether the platform snapshot still exists.
    fn platform_exists(&self, source: &str, handle: &str) -> SysResult<bool> {
        let module = StorageModule::new(self.runner, self.fs);
        let snapshots = module.list(self.backend, source)?;
        Ok(snapshots.iter().any(|snapshot| {
            snapshot.name == handle
                || handle.ends_with(&snapshot.name)
                || snapshot.name.ends_with(handle)
        }))
    }

    /// Deletes the platform snapshot.
    fn delete_platform(&self, handle: &str) -> SysResult<()> {
        let spec = match self.backend {
            SnapshotBackend::Btrfs => {
                CommandSpec::new("btrfs")?.args(["subvolume", "delete", handle])?
            }
            SnapshotBackend::Lvm => CommandSpec::new("lvremove")?.args(["-f", handle])?,
            SnapshotBackend::Zfs => CommandSpec::new("zfs")?.args(["destroy", handle])?,
        };
        let output = self.runner.run(&spec)?;
        output.require_success("deleting the platform snapshot")
    }

    /// Rolls the source back to the platform snapshot.
    fn restore_platform(
        &self,
        source: &str,
        handle: &str,
    ) -> SysResult<(String, bool, Option<String>)> {
        match self.backend {
            SnapshotBackend::Btrfs => {
                let aside = format!("{source}.replaced");
                if self.fs.exists(Path::new(source))? {
                    self.fs.rename(Path::new(source), Path::new(&aside))?;
                }
                let spec =
                    CommandSpec::new("btrfs")?.args(["subvolume", "snapshot", handle, source])?;
                let output = self.runner.run(&spec)?;
                output.require_success("restoring the btrfs snapshot")?;
                Ok((source.to_owned(), true, Some(aside)))
            }
            SnapshotBackend::Lvm => {
                let spec = CommandSpec::new("lvconvert")?.args(["--merge", handle])?;
                let output = self.runner.run(&spec)?;
                output.require_success("merging the LVM snapshot")?;
                Ok((source.to_owned(), false, None))
            }
            SnapshotBackend::Zfs => {
                let spec = CommandSpec::new("zfs")?.args(["rollback", handle])?;
                let output = self.runner.run(&spec)?;
                output.require_success("rolling back the ZFS dataset")?;
                Ok((source.to_owned(), true, None))
            }
        }
    }

    /// The next UUIDv7 snapshot id, derived from the injected clock and a counter (never a
    /// wall-clock read, and valid for [`Snapshot::open`]'s canonical check).
    fn next_id(&self) -> String {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        let now = self.clock.now_utc();
        let millis = u64::try_from(now.unix_timestamp()).unwrap_or(0) * 1000
            + u64::from(now.nanosecond()) / 1_000_000;
        uuid_v7(millis, counter)
    }
}

/// The `SnapshotProvider` implementation: `take`, `restore`, `verify` and `delete` over the
/// platform backend.
impl SnapshotProvider for LinuxSnapshotProvider<'_> {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_online: false,
            needs_stopped_instance: false,
            incremental: matches!(self.backend, SnapshotBackend::Zfs | SnapshotBackend::Btrfs),
            max_parallel: 1,
            estimated_throughput: 0,
        }
    }

    fn name(&self) -> &'static str {
        match self.backend {
            SnapshotBackend::Btrfs => "linux-btrfs",
            SnapshotBackend::Lvm => "linux-lvm",
            SnapshotBackend::Zfs => "linux-zfs",
        }
    }

    fn take(
        &self,
        target: &SnapshotTarget,
        dest: &Destination,
        opts: &SnapshotOptions,
        progress: Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<TakeOutcome>> + Send + '_>> {
        let target = target.clone();
        let dest = dest.clone();
        let opts = opts.clone();
        Box::pin(async move { self.take_inner(&target, &dest, &opts, progress).await })
    }

    fn restore(
        &self,
        snapshot: &Snapshot,
        target: &RestoreTarget,
        opts: &RestoreOptions,
        progress: Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<spot_snapshot::restore::Restored>> + Send + '_>>
    {
        let source = match self.source_of(snapshot) {
            Ok(source) => source,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let handle = match self.handle_of(snapshot) {
            Ok(handle) => handle,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let target = target.clone();
        let opts = opts.clone();
        let snapshot = snapshot.clone();
        Box::pin(async move {
            opts.check_replace(&target)?;
            progress.on(Progress::Started {
                phase: 2,
                files_total: 0,
            });
            let (location, activated, replaced_aside) = self
                .restore_platform(&source, &handle)
                .map_err(to_snapshot_error)?;
            progress.on(Progress::Finished { ok: true });
            Ok(spot_snapshot::restore::Restored {
                snapshot,
                location,
                activated,
                replaced_aside,
            })
        })
    }

    fn verify(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<VerifyReport>> + Send + '_>> {
        let source = self.source_of(snapshot);
        let handle = self.handle_of(snapshot);
        Box::pin(async move {
            let source = source?;
            let handle = handle?;
            if self
                .platform_exists(&source, &handle)
                .map_err(to_snapshot_error)?
            {
                Ok(VerifyReport {
                    files_checked: 0,
                    bytes_checked: 0,
                })
            } else {
                Err(SnapshotError::VerifyFailed {
                    reason: format!("the platform snapshot {handle} is gone"),
                })
            }
        })
    }

    fn delete(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<()>> + Send + '_>> {
        let handle = self.handle_of(snapshot);
        Box::pin(async move {
            let handle = handle?;
            self.delete_platform(&handle).map_err(to_snapshot_error)
        })
    }
}

/// The deepest mount whose target is a prefix of `path`.
#[must_use]
pub fn deepest_mount<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    let target = path.display().to_string();
    mounts
        .iter()
        .flat_map(Mount::flatten)
        .filter(|mount| {
            target == mount.target
                || target.starts_with(&format!("{}/", mount.target.trim_end_matches('/')))
                || mount.target == "/"
        })
        .max_by_key(|mount| mount.target.len())
}

/// Converts a mount source device into an LVM `vg/lv`, when it is one.
#[must_use]
pub fn lvm_from_device(device: &str) -> Option<String> {
    if let Some(rest) = device.strip_prefix("/dev/mapper/") {
        let (vg, lv) = rest.split_once('-')?;
        return Some(format!(
            "{}/{}",
            vg.replace("--", "-"),
            lv.replace("--", "-")
        ));
    }
    if let Some(rest) = device.strip_prefix("/dev/") {
        let (vg, lv) = rest.split_once('/')?;
        if vg == "disk" || vg.is_empty() {
            return None;
        }
        return Some(format!("{vg}/{lv}"));
    }
    None
}

/// Renders a UUIDv7 string from a millisecond timestamp and a counter, without reading a
/// clock: the version nibble is 7 and the variant bits are `10`, so the snapshot manifest's
/// canonical check accepts it.
#[must_use]
pub fn uuid_v7(millis: u64, counter: u64) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    millis.hash(&mut hasher);
    counter.hash(&mut hasher);
    let random = hasher.finish();
    let time_low = ((millis >> 16) & 0xFFFF_FFFF) as u32;
    let time_mid = (millis & 0xFFFF) as u16;
    let version = 0x7000 | ((random >> 48) & 0x0FFF) as u16;
    let variant = 0x8000 | ((random >> 32) & 0x3FFF) as u16;
    let node = random & 0xFFFF_FFFF_FFFF;
    format!("{time_low:08x}-{time_mid:04x}-{version:04x}-{variant:04x}-{node:012x}")
}

/// The short form of an id, for a platform snapshot name.
fn short_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect()
}

/// Converts a module error into the provider contract's error.
fn to_snapshot_error(error: SysError) -> SnapshotError {
    SnapshotError::io(format!("{}: {}", error.code, error.message))
}

/// The conventional [`SNAPSHOT_MISSING`] failure, for a caller of the module API.
#[must_use]
pub fn snapshot_missing(handle: &str) -> SysError {
    SysError::new(
        SNAPSHOT_MISSING,
        format!("the snapshot {handle} does not exist"),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn uuid_v7_has_the_version_and_variant() {
        let id = uuid_v7(1_700_000_000_000, 0);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].starts_with('7'));
        assert!(matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'b')
        ));
        assert_eq!(parts[4].len(), 12);
        assert_ne!(uuid_v7(1, 0), uuid_v7(1, 1));
    }

    #[test]
    fn lvm_devices_convert_to_vg_lv() {
        assert_eq!(
            lvm_from_device("/dev/mapper/vg0-root"),
            Some("vg0/root".to_owned())
        );
        assert_eq!(
            lvm_from_device("/dev/vg0/root"),
            Some("vg0/root".to_owned())
        );
        assert_eq!(lvm_from_device("/dev/sda1"), None);
    }

    #[test]
    fn the_deepest_mount_wins() {
        let mounts = crate::facts::parse_findmnt(
            r#"{"filesystems":[{"target":"/","source":"/dev/sda1","fstype":"xfs"},{"target":"/var/lib","source":"tank/data","fstype":"zfs"}]}"#,
        )
        .unwrap();
        let found = deepest_mount(&mounts, Path::new("/var/lib/postgresql/18")).unwrap();
        assert_eq!(found.source, "tank/data");
    }

    #[test]
    fn a_database_target_is_refused() {
        let error = LinuxSnapshotProvider::target_path(&SnapshotTarget::Database {
            connection: "postgres://localhost/x".to_owned(),
            tenant: "t".to_owned(),
            database: "x".to_owned(),
        })
        .unwrap_err();
        assert_eq!(error.code, SNAPSHOT_TARGET);
    }
}
