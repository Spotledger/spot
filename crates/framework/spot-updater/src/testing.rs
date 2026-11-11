//! In-memory fakes for the update seams, with failure and power-cut injection.
//!
//! Available to this crate's tests and, with the `testing` feature, to any caller that wants to
//! rehearse an update without touching a real disk. [`FakeFileSystem`] models a durable disk
//! whose writes are atomic (a write either happens whole or not at all), and can be told to
//! fail at an operation, to run out of space, or to power off after `n` operations — the three
//! ways the crash-resume tests need to cut the engine. [`FakeService`] and [`FakeDataHooks`]
//! record their calls and can fail each one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::UpdaterError;
use crate::fs::FileSystem;
use crate::hooks::DataHooks;
use crate::layout::Layout;
use crate::service::ServiceControl;
use semver::Version;

/// The mutable state of a [`FakeFileSystem`], behind one lock.
struct Disk {
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: BTreeSet<PathBuf>,
    used: u64,
    ops: u64,
    cut_after: Option<u64>,
    fail_at: Option<(u64, UpdaterError)>,
    capacity: Option<u64>,
    free_report: Option<u64>,
    durable_writes: u64,
}

/// An in-memory filesystem with failure and power-cut injection.
pub struct FakeFileSystem {
    disk: Mutex<Disk>,
}

impl Default for FakeFileSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeFileSystem {
    /// An empty filesystem.
    #[must_use]
    pub fn new() -> Self {
        Self {
            disk: Mutex::new(Disk {
                files: BTreeMap::new(),
                dirs: BTreeSet::new(),
                used: 0,
                ops: 0,
                cut_after: None,
                fail_at: None,
                capacity: None,
                free_report: None,
                durable_writes: 0,
            }),
        }
    }

    /// Creates a directory and its parents, as a test set-up step (not counted as an operation).
    pub fn mkdir(&self, path: &Path) {
        let mut disk = self.lock();
        add_dir(&mut disk, path);
    }

    /// Writes a file, as a test set-up step (not counted as an operation).
    pub fn put_file(&self, path: &Path, bytes: &[u8]) {
        let mut disk = self.lock();
        if let Some(parent) = path.parent() {
            add_dir(&mut disk, parent);
        }
        disk.used = disk.used.saturating_add(bytes.len() as u64);
        disk.files.insert(path.to_path_buf(), bytes.to_vec());
    }

    /// Seeds a complete version directory with the given `(relative path, bytes)` files.
    pub fn seed_version(&self, layout: &Layout, version: &Version, files: &[(&str, &[u8])]) {
        let dir = layout.version_dir(version);
        self.mkdir(&dir);
        for (relative, bytes) in files {
            self.put_file(&dir.join(relative), bytes);
        }
    }

    /// Puts the `current` pointer at `version`.
    pub fn seed_current(&self, layout: &Layout, version: &Version) {
        self.put_file(&layout.current_path(), version.to_string().as_bytes());
    }

    /// The file's bytes, if it exists.
    #[must_use]
    pub fn file(&self, path: &Path) -> Option<Vec<u8>> {
        self.lock().files.get(path).cloned()
    }

    /// The version named by a pointer file, if it is present and parses.
    #[must_use]
    pub fn pointer(&self, path: &Path) -> Option<Version> {
        let bytes = self.file(path)?;
        Version::parse(std::str::from_utf8(&bytes).ok()?.trim()).ok()
    }

    /// Whether a directory exists.
    #[must_use]
    pub fn has_dir(&self, path: &Path) -> bool {
        self.lock().dirs.contains(path)
    }

    /// A boundary shared with service and hook fakes, before and after their side effects.
    ///
    /// # Errors
    /// Returns the configured failure or power-cut error at this operation.
    pub fn checkpoint(&self) -> Result<(), UpdaterError> {
        Self::op(&mut self.lock())
    }

    /// How many filesystem operations have run so far.
    #[must_use]
    pub fn ops(&self) -> u64 {
        self.lock().ops
    }

    /// How many durable writes have happened.
    #[must_use]
    pub fn durable_writes(&self) -> u64 {
        self.lock().durable_writes
    }

    /// Cuts the power after `n` successful operations: operation `n + 1` returns
    /// [`UpdaterError::PowerCut`], and every operation after that, until [`reboot`] is called.
    ///
    /// [`reboot`]: FakeFileSystem::reboot
    pub fn cut_after(&self, n: u64) {
        self.lock().cut_after = Some(n);
    }

    /// Makes operation number `op` (1-based) fail with `error`.
    pub fn fail_at(&self, op: u64, error: UpdaterError) {
        self.lock().fail_at = Some((op, error));
    }

    /// Caps the filesystem at `total_bytes`: a write past it fails with
    /// [`UpdaterError::DiskFull`], and [`free_space`](FileSystem::free_space) reports the room
    /// left.
    pub fn disk_full_at(&self, total_bytes: u64) {
        self.lock().capacity = Some(total_bytes);
    }

    /// Reports `bytes` of free space from [`free_space`](FileSystem::free_space), independent of
    /// the write cap, so a test can let the pre-unpack guard pass and then run the disk dry.
    pub fn report_free_space(&self, bytes: u64) {
        self.lock().free_report = Some(bytes);
    }

    /// The disk survives, the injections do not: clears `cut_after`, `fail_at` and the operation
    /// counter, as a restart after a power cut would.
    pub fn reboot(&self) {
        let mut disk = self.lock();
        disk.cut_after = None;
        disk.fail_at = None;
        disk.ops = 0;
    }

    /// The bytes stored.
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.lock().used
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Disk> {
        self.disk
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Counts one operation and applies the cut/fail injection; the mutation is only passed to
    /// an operation that survives this check.
    fn op(disk: &mut Disk) -> Result<(), UpdaterError> {
        disk.ops = disk.ops.saturating_add(1);
        if let Some((at, error)) = &disk.fail_at
            && disk.ops == *at
        {
            return Err(error.clone());
        }
        if let Some(n) = disk.cut_after
            && disk.ops > n
        {
            return Err(UpdaterError::PowerCut);
        }
        Ok(())
    }
}

/// Adds `path` and every ancestor to the directory set.
fn add_dir(disk: &mut Disk, path: &Path) {
    let mut current = Some(path.to_path_buf());
    while let Some(dir) = current {
        disk.dirs.insert(dir.clone());
        current = dir.parent().map(Path::to_path_buf);
    }
}

/// Whether `candidate` is `ancestor` or sits under it.
fn under(candidate: &Path, ancestor: &Path) -> bool {
    candidate == ancestor || candidate.starts_with(ancestor)
}

impl FileSystem for FakeFileSystem {
    fn read(&self, path: &Path) -> Result<Vec<u8>, UpdaterError> {
        let mut disk = self.lock();
        Self::op(&mut disk)?;
        disk.files
            .get(path)
            .cloned()
            .ok_or_else(|| UpdaterError::Io {
                detail: format!("read {}: not found", path.display()),
            })
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), UpdaterError> {
        self.write(path, bytes, false)
    }

    fn durable_write(&self, path: &Path, bytes: &[u8]) -> Result<(), UpdaterError> {
        self.write(path, bytes, true)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), UpdaterError> {
        let mut disk = self.lock();
        Self::op(&mut disk)?;
        if !disk.files.contains_key(from) && !disk.dirs.contains(from) {
            return Err(UpdaterError::Io {
                detail: format!("rename {}: not found", from.display()),
            });
        }
        let moved_files: Vec<(PathBuf, Vec<u8>)> = disk
            .files
            .iter()
            .filter(|(path, _)| under(path, from))
            .map(|(path, bytes)| (relocate(from, to, path), bytes.clone()))
            .collect();
        let moved_dirs: Vec<PathBuf> = disk
            .dirs
            .iter()
            .filter(|path| under(path, from))
            .map(|path| relocate(from, to, path))
            .collect();
        // Overwrite whatever `to` names, then the source, then place the moved subtree.
        remove_subtree(&mut disk, to);
        remove_subtree(&mut disk, from);
        for dir in &moved_dirs {
            add_dir(&mut disk, dir);
        }
        add_dir(&mut disk, to);
        for (path, bytes) in moved_files {
            disk.used = disk.used.saturating_add(bytes.len() as u64);
            if let Some(parent) = path.parent() {
                add_dir(&mut disk, parent);
            }
            disk.files.insert(path, bytes);
        }
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), UpdaterError> {
        let mut disk = self.lock();
        Self::op(&mut disk)?;
        remove_subtree(&mut disk, path);
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<(), UpdaterError> {
        let mut disk = self.lock();
        Self::op(&mut disk)?;
        if let Some(bytes) = disk.files.remove(path) {
            disk.used = disk.used.saturating_sub(bytes.len() as u64);
        }
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), UpdaterError> {
        let mut disk = self.lock();
        Self::op(&mut disk)?;
        add_dir(&mut disk, path);
        Ok(())
    }

    fn exists(&self, path: &Path) -> Result<bool, UpdaterError> {
        let mut disk = self.lock();
        Self::op(&mut disk)?;
        Ok(disk.files.contains_key(path) || disk.dirs.contains(path))
    }

    fn list(&self, path: &Path) -> Result<Vec<String>, UpdaterError> {
        let mut disk = self.lock();
        Self::op(&mut disk)?;
        let mut names: BTreeSet<String> = BTreeSet::new();
        for candidate in disk.files.keys().chain(disk.dirs.iter()) {
            if let Ok(relative) = candidate.strip_prefix(path)
                && let Some(first) = relative.iter().next()
            {
                names.insert(first.to_string_lossy().into_owned());
            }
        }
        Ok(names.into_iter().collect())
    }

    fn free_space(&self, _path: &Path) -> Result<u64, UpdaterError> {
        let mut disk = self.lock();
        Self::op(&mut disk)?;
        if let Some(report) = disk.free_report {
            return Ok(report);
        }
        Ok(match disk.capacity {
            Some(capacity) => capacity.saturating_sub(disk.used),
            None => u64::MAX,
        })
    }
}

impl FakeFileSystem {
    /// The shared body of both writes: atomic, size-checked, and injection-aware.
    fn write(&self, path: &Path, bytes: &[u8], durable: bool) -> Result<(), UpdaterError> {
        let mut disk = self.lock();
        Self::op(&mut disk)?;
        let parent_ok = path
            .parent()
            .is_none_or(|parent| disk.dirs.contains(parent));
        if !parent_ok {
            return Err(UpdaterError::Io {
                detail: format!("write {}: the parent directory is missing", path.display()),
            });
        }
        let old = disk.files.get(path).map_or(0, Vec::len);
        let used = disk
            .used
            .saturating_sub(old as u64)
            .saturating_add(bytes.len() as u64);
        if let Some(capacity) = disk.capacity
            && used > capacity
        {
            return Err(UpdaterError::DiskFull {
                detail: format!("write {}", path.display()),
            });
        }
        disk.used = used;
        disk.files.insert(path.to_path_buf(), bytes.to_vec());
        if durable {
            disk.durable_writes = disk.durable_writes.saturating_add(1);
        }
        Ok(())
    }
}

/// Removes `path` and everything under it from the disk.
fn remove_subtree(disk: &mut Disk, path: &Path) {
    let removed: u64 = disk
        .files
        .iter()
        .filter(|(candidate, _)| under(candidate, path))
        .map(|(_, bytes)| bytes.len() as u64)
        .sum();
    disk.files.retain(|candidate, _| !under(candidate, path));
    disk.dirs.retain(|candidate| !under(candidate, path));
    disk.used = disk.used.saturating_sub(removed);
}

/// Remaps a directory set through `remap`, for the rename operation.
fn relocate(from: &Path, to: &Path, path: &Path) -> PathBuf {
    let relative = path.strip_prefix(from).unwrap_or_else(|_| Path::new(""));
    if relative.as_os_str().is_empty() {
        to.to_path_buf()
    } else {
        to.join(relative)
    }
}

/// The mutable state of a [`FakeService`].
struct Service {
    running: Option<Version>,
    ready: bool,
    stop_fails: bool,
    start_fails: bool,
    ready_fails: bool,
    fail_start_once: bool,
    ready_fail_once: bool,
    unhealthy: bool,
    started: Vec<Version>,
    stops: u32,
}

/// An in-memory service controller.
pub struct FakeService {
    faults: Option<Arc<FakeFileSystem>>,
    service: Mutex<Service>,
}

impl FakeService {
    /// A service running `version`, ready and healthy.
    #[must_use]
    pub fn new(version: Option<Version>) -> Self {
        Self {
            faults: None,
            service: Mutex::new(Service {
                running: version,
                ready: true,
                stop_fails: false,
                start_fails: false,
                ready_fails: false,
                fail_start_once: false,
                ready_fail_once: false,
                unhealthy: false,
                started: Vec::new(),
                stops: 0,
            }),
        }
    }

    /// A service that is not running (as after a power cut).
    #[must_use]
    pub fn down() -> Self {
        Self::new(None)
    }

    /// Makes [`stop`](ServiceControl::stop) fail.
    pub fn fail_stop(&self) {
        self.lock().stop_fails = true;
    }

    /// Makes [`start`](ServiceControl::start) fail.
    pub fn fail_start(&self) {
        self.lock().start_fails = true;
    }

    /// Fails the next [`start`](ServiceControl::start) only, so a rollback's start can succeed.
    pub fn fail_next_start(&self) {
        self.lock().fail_start_once = true;
    }

    /// Fails the next [`wait_ready`](ServiceControl::wait_ready) only.
    pub fn not_ready_once(&self) {
        self.lock().ready_fail_once = true;
    }

    /// Makes the service never become ready: [`wait_ready`](ServiceControl::wait_ready) fails.
    pub fn never_ready(&self) {
        self.lock().ready = false;
    }

    /// Makes [`is_healthy`](ServiceControl::is_healthy) report unhealthy.
    pub fn unhealthy(&self) {
        self.lock().unhealthy = true;
    }

    /// Marks the service as crashed (not running, unhealthy), after a commit.
    pub fn crash(&self) {
        let mut service = self.lock();
        service.running = None;
        service.unhealthy = true;
    }

    /// The version currently running, if any.
    #[must_use]
    pub fn running(&self) -> Option<Version> {
        self.lock().running.clone()
    }

    /// Every version that was started, in order.
    #[must_use]
    pub fn started(&self) -> Vec<Version> {
        self.lock().started.clone()
    }

    /// How many times the service was stopped.
    #[must_use]
    pub fn stops(&self) -> u32 {
        self.lock().stops
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Service> {
        self.service
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl FakeService {
    /// Shares the disk's operation counter and cuts, including after each side effect.
    #[must_use]
    pub fn with_faults(mut self, fs: Arc<FakeFileSystem>) -> Self {
        self.faults = Some(fs);
        self
    }

    fn checkpoint(&self) -> Result<(), UpdaterError> {
        self.faults.as_ref().map_or(Ok(()), |fs| fs.checkpoint())
    }
}

impl ServiceControl for FakeService {
    fn stop(&self) -> Result<(), UpdaterError> {
        self.checkpoint()?;
        let mut service = self.lock();
        if service.stop_fails {
            return Err(UpdaterError::Service {
                detail: "stop failed".to_owned(),
            });
        }
        service.running = None;
        service.stops = service.stops.saturating_add(1);
        drop(service);
        self.checkpoint()
    }

    fn start(&self, version: &Version) -> Result<(), UpdaterError> {
        self.checkpoint()?;
        let mut service = self.lock();
        if service.start_fails {
            return Err(UpdaterError::Service {
                detail: "start failed".to_owned(),
            });
        }
        if service.fail_start_once {
            service.fail_start_once = false;
            return Err(UpdaterError::Service {
                detail: "start failed once".to_owned(),
            });
        }
        service.running = Some(version.clone());
        service.started.push(version.clone());
        drop(service);
        self.checkpoint()
    }

    fn wait_ready(&self, deadline: i64) -> Result<(), UpdaterError> {
        self.checkpoint()?;
        let mut service = self.lock();
        if service.ready_fail_once {
            service.ready_fail_once = false;
            return Err(UpdaterError::Health {
                detail: format!("not ready by {deadline}"),
            });
        }
        if service.ready_fails || !service.ready {
            return Err(UpdaterError::Health {
                detail: format!("not ready by {deadline}"),
            });
        }
        drop(service);
        self.checkpoint()
    }

    fn is_healthy(&self) -> Result<bool, UpdaterError> {
        self.checkpoint()?;
        let service = self.lock();
        let healthy = service.running.is_some() && !service.unhealthy;
        drop(service);
        self.checkpoint()?;
        Ok(healthy)
    }
}

/// The mutable state of a [`FakeDataHooks`].
struct Hooks {
    epoch: u64,
    backup_epoch: Option<u64>,
    backup_fails: bool,
    migrate_fails: bool,
    restore_fails: bool,
    backups: u32,
    migrations: u32,
    restores: u32,
}

/// In-memory data hooks that record their calls and can fail each one.
pub struct FakeDataHooks {
    faults: Option<Arc<FakeFileSystem>>,
    hooks: Mutex<Hooks>,
}

impl Default for FakeDataHooks {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDataHooks {
    /// Hooks whose every step succeeds.
    #[must_use]
    pub fn new() -> Self {
        Self {
            faults: None,
            hooks: Mutex::new(Hooks {
                epoch: 2,
                backup_epoch: None,
                backup_fails: false,
                migrate_fails: false,
                restore_fails: false,
                backups: 0,
                migrations: 0,
                restores: 0,
            }),
        }
    }

    /// Makes [`backup`](DataHooks::backup) fail.
    pub fn fail_backup(&self) {
        self.lock().backup_fails = true;
    }

    /// Makes [`migrate`](DataHooks::migrate) fail.
    pub fn fail_migrate(&self) {
        self.lock().migrate_fails = true;
    }

    /// Makes [`restore`](DataHooks::restore) fail.
    pub fn fail_restore(&self) {
        self.lock().restore_fails = true;
    }

    /// How many backups were taken.
    #[must_use]
    pub fn backups(&self) -> u32 {
        self.lock().backups
    }

    /// How many migrations ran.
    #[must_use]
    pub fn migrations(&self) -> u32 {
        self.lock().migrations
    }

    /// How many restores ran.
    #[must_use]
    pub fn restores(&self) -> u32 {
        self.lock().restores
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Hooks> {
        self.hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl FakeDataHooks {
    /// Shares the disk's operation counter and cuts before and after hook effects.
    #[must_use]
    pub fn with_faults(mut self, fs: Arc<FakeFileSystem>) -> Self {
        self.faults = Some(fs);
        self
    }

    /// The durable fake database schema epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    fn checkpoint(&self) -> Result<(), UpdaterError> {
        self.faults.as_ref().map_or(Ok(()), |fs| fs.checkpoint())
    }
}

impl DataHooks for FakeDataHooks {
    fn backup(&self) -> Result<(), UpdaterError> {
        self.checkpoint()?;
        let mut hooks = self.lock();
        if hooks.backup_fails {
            return Err(UpdaterError::Hook {
                detail: "backup failed".to_owned(),
            });
        }
        let epoch = hooks.epoch;
        hooks.backup_epoch = Some(epoch);
        hooks.backups = hooks.backups.saturating_add(1);
        drop(hooks);
        self.checkpoint()
    }

    fn migrate(&self, _from_epoch: u64, to_epoch: u64) -> Result<(), UpdaterError> {
        self.checkpoint()?;
        let mut hooks = self.lock();
        if hooks.migrate_fails {
            return Err(UpdaterError::Hook {
                detail: "migrate failed".to_owned(),
            });
        }
        hooks.epoch = to_epoch;
        hooks.migrations = hooks.migrations.saturating_add(1);
        drop(hooks);
        self.checkpoint()
    }

    fn restore(&self) -> Result<(), UpdaterError> {
        self.checkpoint()?;
        let mut hooks = self.lock();
        if hooks.restore_fails {
            return Err(UpdaterError::Hook {
                detail: "restore failed".to_owned(),
            });
        }
        if let Some(epoch) = hooks.backup_epoch {
            hooks.epoch = epoch;
        }
        hooks.restores = hooks.restores.saturating_add(1);
        drop(hooks);
        self.checkpoint()
    }
}
