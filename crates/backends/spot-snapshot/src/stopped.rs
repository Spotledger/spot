//! The stopped-instance provider: a fast offline snapshot of one managed instance's
//! data directory, taken while the instance is stopped through
//! [`spot_pg_supervisor`], and the atomic restore of it.
//!
//! Take: stop (the supervisor's own stop, never a kill by this crate), copy with
//! per-file SHA-256 while a guard owns the stop, start again — **even when the copy
//! fails** (the guard's `Drop` restarts; `finish` restarts on success; the failure
//! path is tested). Cancel the take at any await: the guard drops, the instance
//! restarts, and the staged `.partial` neighbour stays open-able nothing.
//!
//! Restore: for a stopped instance, rename the old data directory aside, copy the
//! snapshot in (verified), then switch atomically; the old directory is never dropped
//! here.
//!
//! Tests need a bundled layout (`SPOT_TEST_PG_BIN`) and are gated accordingly; see
//! the README.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::conn::unix_ms;
use crate::error::{SnapshotError, SnapshotResult};
use crate::hash;
use crate::manifest::{
    Consistency, DestinationPath, Snapshot, SnapshotManifest, check_destination, staging_of,
};
use crate::provider::{
    Capabilities, Destination, Progress, ProgressSink, RestoreOptions, RestoreTarget,
    SnapshotOptions, SnapshotProvider, SnapshotTarget, TakeOutcome, VerifyReport,
};
use crate::restore::Restored;

/// The stop grace the provider gives the supervisor's stop (bounded, rule 12).
const STOP_GRACE: Duration = Duration::from_secs(30);

/// The restart budget window config: short, so a failed copy's restart is prompt.
fn supervisor_config() -> spot_pg_supervisor::config::SupervisorConfig {
    spot_pg_supervisor::config::SupervisorConfig {
        start_timeout: Duration::from_secs(60),
        stop_grace: STOP_GRACE,
        ..spot_pg_supervisor::config::SupervisorConfig::default()
    }
}

/// The stopped-instance provider over one managed PostgreSQL instance.
#[derive(Clone)]
pub struct StoppedInstanceProvider {
    /// The injected clock.
    clock: Arc<dyn spot_core::Clock>,
}

impl std::fmt::Debug for StoppedInstanceProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("StoppedInstanceProvider").finish()
    }
}

impl StoppedInstanceProvider {
    /// A provider with the injected clock.
    pub fn new(clock: Arc<dyn spot_core::Clock>) -> Self {
        StoppedInstanceProvider { clock }
    }
}

/// Whether a bundled layout test can run here: `SPOT_TEST_PG_BIN` names the layout
/// root (its `bin` subdirectory holds the binaries). The gate is documented in the
/// README: no runner ships a bundled PostgreSQL.
pub fn bundled_layout_for_tests() -> Option<spot_pg_supervisor::layout::PgLayout> {
    let root = std::env::var("SPOT_TEST_PG_BIN").ok()?;
    if root.is_empty() {
        return None;
    }
    let root = PathBuf::from(root);
    Some(spot_pg_supervisor::layout::PgLayout {
        bin_dir: root.join("bin"),
        data_dir: root.join("data"),
        log_dir: root.join("logs"),
        ..spot_pg_supervisor::layout::PgLayout::default()
    })
}

/// Parked supervision-round handles, for the whole process lifetime. Dropping a
/// supervisor `Handle` closes its stop channel; the monitor reads that as
/// "supervisor gone" and answers by killing the child. A restarted instance must keep
/// running after the operation that restarted it ends — the caller may drop its
/// provider at any moment — so every restart's Handle is parked in this registry and
/// lives as long as the process (one Handle per take or cancel; process exit cleans
/// up, as with every child the death guarantees own).
static PARKED_HANDLES: std::sync::OnceLock<
    std::sync::Mutex<Vec<spot_pg_supervisor::supervisor::Handle>>,
> = std::sync::OnceLock::new();

/// Parks one handle so its monitor (and the child it owns) stays alive.
fn park_handle(handle: spot_pg_supervisor::supervisor::Handle) {
    let parked = PARKED_HANDLES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if let Ok(mut parked) = parked.lock() {
        parked.push(handle);
    }
}

/// The guard owning one stopped instance for the duration of a copy. `Drop` and
/// [`StopGuard::finish`] both restart the instance through the supervisor — the
/// instance comes back even when the copy fails or the take is cancelled (ADR-068
/// rule 26: the recovery path always runs; the brief's explicit demand).
struct StopGuard {
    supervisor: Arc<spot_pg_supervisor::supervisor::Supervisor>,
    done: bool,
}

impl StopGuard {
    /// Stops the instance and returns the guard that will restart it.
    async fn stop(
        supervisor: Arc<spot_pg_supervisor::supervisor::Supervisor>,
    ) -> SnapshotResult<StopGuard> {
        let handle = supervisor.start().await.map_err(map_supervisor)?;
        let how = handle.stop(STOP_GRACE).await.map_err(map_supervisor)?;
        let _ = how;
        Ok(StopGuard {
            supervisor,
            done: false,
        })
    }

    /// Restarts the instance and marks the guard done (the explicit path; `Drop`
    /// would restart too, so calling this twice is safe).
    async fn finish(mut self) -> SnapshotResult<()> {
        self.done = true;
        self.restart().await
    }

    /// The restart itself.
    async fn restart(&self) -> SnapshotResult<()> {
        // `start` refuses a live postmaster and returns a handle; we wait for ready
        // so the caller's next operation talks to a running instance. The handle is
        // parked, never dropped: dropping it would kill the restarted child as
        // "supervisor gone".
        let handle = self.supervisor.start().await.map_err(map_supervisor)?;
        handle.ready().await.map_err(map_supervisor)?;
        park_handle(handle);
        Ok(())
    }
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // The failure/cancellation path: restart on a dedicated OS thread with its
        // own runtime, so the instance comes back even when the take future was
        // cancelled at an arbitrary await (no runtime may exist here). The runtime
        // must outlive the restart: `start` spawns the monitor task onto it, and
        // with `kill_on_drop` the monitor owns the child. So after the instance is
        // ready the thread blocks inside `block_on` forever — the monitor keeps
        // running (crash restarts included) instead of dying with the runtime.
        // Only process exit ends this; the leak is one thread per aborted take. The
        // handle is parked in the process-lifetime registry before the thread parks,
        // so the round's ownership story matches the explicit path's.
        let supervisor = self.supervisor.clone();
        let _ = std::thread::Builder::new()
            .name("spot-snapshot-guard-restart".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(runtime) = runtime {
                    let ready = runtime.block_on(async move {
                        match supervisor.start().await {
                            Ok(handle) => match handle.ready().await {
                                Ok(()) => {
                                    park_handle(handle);
                                    true
                                }
                                Err(_) => false,
                            },
                            Err(_) => false,
                        }
                    });
                    if ready {
                        // Keep polling the monitor (and with it the child) for this
                        // supervision round: a `pending` future inside `block_on`
                        // parks the thread while the runtime drives its tasks.
                        runtime.block_on(std::future::pending::<()>());
                    }
                    // Not ready: the monitor already published the failure and ended;
                    // dropping the runtime drops nothing that is still alive.
                }
            });
    }
}

fn map_supervisor(e: spot_pg_supervisor::error::SupervisorError) -> SnapshotError {
    SnapshotError::io(format!("the instance supervisor failed: {e}"))
}

impl SnapshotProvider for StoppedInstanceProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_online: false,
            needs_stopped_instance: true,
            incremental: false,
            max_parallel: 4,
            estimated_throughput: 0,
        }
    }

    fn name(&self) -> &'static str {
        "stopped"
    }

    fn take(
        &self,
        target: &SnapshotTarget,
        dest: &Destination,
        opts: &SnapshotOptions,
        progress: std::sync::Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<TakeOutcome>> + Send + '_>> {
        let clock: Arc<dyn spot_core::Clock> = self.clock.clone();
        let target = target.clone();
        let dest = dest.clone();
        let opts = opts.clone();
        Box::pin(async move { take_stopped(&target, &dest, &opts, clock, &*progress).await })
    }

    fn restore(
        &self,
        snapshot: &Snapshot,
        target: &RestoreTarget,
        opts: &RestoreOptions,
        progress: std::sync::Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<Restored>> + Send + '_>> {
        let clock: Arc<dyn spot_core::Clock> = self.clock.clone();
        let snapshot = snapshot.clone();
        let target = target.clone();
        let opts = opts.clone();
        Box::pin(async move { restore_stopped(&snapshot, &target, &opts, clock, &*progress).await })
    }

    fn verify(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<VerifyReport>> + Send + '_>> {
        let snapshot = snapshot.clone();
        Box::pin(async move { snapshot.verify().await })
    }

    fn delete(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<()>> + Send + '_>> {
        let dir = snapshot.dir().to_path_buf();
        Box::pin(async move {
            tokio::fs::remove_dir_all(&dir)
                .await
                .map_err(SnapshotError::from)
        })
    }
}

/// The take: stop, copy with checksums, restart always, manifest last.
async fn take_stopped(
    target: &SnapshotTarget,
    dest: &Destination,
    opts: &SnapshotOptions,
    clock: Arc<dyn spot_core::Clock>,
    progress: &dyn ProgressSink,
) -> SnapshotResult<TakeOutcome> {
    let _ = opts;
    let SnapshotTarget::Instance {
        layout,
        tenant,
        password,
    } = target
    else {
        return Err(SnapshotError::io(
            "the stopped-instance provider takes snapshots of instances, not of databases",
        ));
    };
    check_destination(&DestinationPath(dest.dir.clone())).await?;
    let started_at = clock.now_utc();
    progress.on(Progress::Started {
        phase: 1,
        files_total: 0,
    });

    let supervisor = Arc::new(spot_pg_supervisor::supervisor::Supervisor::new(
        layout.clone(),
        spot_pg_supervisor::settings::PgSettings::default(),
        supervisor_config(),
    ));

    // The instance must have been started once by the caller (the bundled model);
    // stop it for the copy. The guard owns the stop from here on.
    let guard = StopGuard::stop(supervisor.clone()).await?;

    // Stage into `<dest>.partial`, copy with checksums, manifest last — never a
    // snapshot `Snapshot::open` accepts until then. The staging tree IS the data
    // dir's image (copied to the staging root), so the manifest's inventory keys
    // (relative to the staging root, exactly the paths `verify` and `restore`
    // join against the snapshot dir) match the on-disk layout. Staging lives
    // beside the destination: a mid-copy failure (or a cancel) removes it here so
    // the caller's disk is never left holding an unfinished take; the guard's
    // restart, on the other hand, is never skipped (the Drop path runs on its own
    // thread).
    let staged = staging_of(&dest.dir);
    let _ = tokio::fs::remove_dir_all(&staged).await;
    tokio::fs::create_dir_all(&staged).await?;
    let copy = async { hash::copy_tree_checked(&layout.data_dir, &staged, progress).await };
    let copied = copy.await;
    // Restart first, success or not (the brief's order; the guard's Drop restarts on
    // cancellation too). Neither failure may swallow the other: when the copy failed
    // AND the restart failed, both reasons surface in one error. A failed copy also
    // takes its staged tree with it.
    let restarted = guard.finish().await;
    let inventory = match copied {
        Ok(inventory) => inventory,
        Err(copy_error) => {
            let _ = tokio::fs::remove_dir_all(&staged).await;
            if let Err(restart_error) = restarted {
                return Err(SnapshotError::io(format!(
                    "the copy failed ({copy_error}); additionally, restarting the instance failed: {restart_error}"
                )));
            }
            return Err(copy_error);
        }
    };
    let finished_at = clock.now_utc();
    let manifest = SnapshotManifest {
        id: uuid::Uuid::now_v7().to_string(),
        tenant: tenant.clone(),
        provider: crate::PROVIDER_NAME.to_owned(),
        provider_version: crate::PROVIDER_VERSION.to_owned(),
        provider_kind: "stopped".to_owned(),
        pg_major: read_pg_major(&staged)?,
        catalogue_version: String::new(),
        feed_schema: String::new(),
        outbox_generation: 0,
        outbox_last_published_seq: 0,
        started_at: SnapshotManifest::format_instant(started_at),
        finished_at: SnapshotManifest::format_instant(finished_at),
        consistency: Consistency::Offline,
        total_bytes: inventory
            .values()
            .try_fold(0u64, |sum, file| Some(sum.saturating_add(file.bytes)))
            .unwrap_or(u64::MAX),
        file_count: u64::try_from(inventory.len()).unwrap_or(u64::MAX),
        files: inventory,
        node_local: Default::default(),
        platform_extra: Default::default(),
    };
    manifest.publish(&dest.dir).await?;
    let _ = password;
    restarted.map_err(|e| {
        SnapshotError::io(format!(
            "the instance restarted with a pending failure: {e}"
        ))
    })?;
    progress.on(Progress::Finished { ok: true });
    Ok(TakeOutcome {
        snapshot: Snapshot::open(&dest.dir).await?,
    })
}

/// Reads a data dir's `PG_VERSION` into the major number, refusing other majors.
fn read_pg_major(data_dir: &Path) -> SnapshotResult<u32> {
    let text = std::fs::read_to_string(data_dir.join("PG_VERSION"))
        .map_err(|e| SnapshotError::io(format!("cannot read PG_VERSION: {e}")))?;
    let major: u32 = text
        .trim()
        .parse()
        .map_err(|_| SnapshotError::io("PG_VERSION does not hold a major number"))?;
    if major != crate::SUPPORTED_MAJOR_VERSION {
        return Err(SnapshotError::VersionMismatch {
            snapshot_major: major,
            target_major: crate::SUPPORTED_MAJOR_VERSION,
        });
    }
    Ok(major)
}

/// The restore: rename the stopped instance's data dir aside, copy the snapshot in
/// (verified), switch atomically. The instance must be stopped (refused otherwise);
/// the caller restarts it after the switch, or uses the guard in `restore_and_check`.
#[allow(clippy::too_many_arguments)]
async fn restore_stopped(
    snapshot: &Snapshot,
    target: &RestoreTarget,
    opts: &RestoreOptions,
    clock: Arc<dyn spot_core::Clock>,
    progress: &dyn ProgressSink,
) -> SnapshotResult<Restored> {
    opts.check_replace(target)?;
    let RestoreTarget::Instance { layout, .. } = target else {
        return Err(SnapshotError::io(
            "the stopped-instance provider restores into instances, not into databases",
        ));
    };
    snapshot.verify().await?;
    progress.on(Progress::Started {
        phase: 2,
        files_total: 0,
    });

    // Refuse a live instance: only a stopped one may be replaced (the guard's rule).
    // The pid file's first line names the postmaster; only a pid that is ALIVE means
    // running. A stale file (a dead pid, exactly what a stopped instance's previous
    // generation leaves behind) is no obstacle: the next start's stale-lock gate
    // cleans it, and the rename-aside keeps it with the moved copy.
    let pid_path = layout.data_dir.join(concat!("postmaster", ".pid"));
    if let Some(pid) = read_pid_first_line(&pid_path).await
        && pid_alive_here(pid)
    {
        return Err(SnapshotError::io(format!(
            "the instance at {} is running ({} names pid {pid}); stop it before restoring",
            layout.data_dir.display(),
            pid_path.display()
        )));
    }
    // The data dir itself must exist: a restore into a fresh layout (or a removed
    // data dir) copies into the target path afterwards.
    if !tokio::fs::try_exists(&layout.data_dir)
        .await
        .unwrap_or(false)
    {
        return Err(SnapshotError::io(format!(
            "the instance data dir {} does not exist (initialise the instance first)",
            layout.data_dir.display()
        )));
    }
    if !opts.replace
        && tokio::fs::try_exists(&layout.data_dir)
            .await
            .unwrap_or(false)
        && has_entries(&layout.data_dir).await
    {
        return Err(SnapshotError::ReplaceRequired {
            target: layout.data_dir.display().to_string(),
        });
    }
    let aside_path = if opts.replace {
        Some(rename_dir_aside(&layout.data_dir, unix_ms(&clock)).await?)
    } else {
        None
    };

    // Copy into a staging sibling, verify, then switch atomically (rename).
    let staged = staging_of(&layout.data_dir);
    let _ = tokio::fs::remove_dir_all(&staged).await;
    let copied = async {
        let inventory: hash::Inventory = snapshot.manifest().files.clone();
        hash::copy_manifest_files(snapshot.dir(), &inventory, &staged, "", progress).await
    };
    if let Err(e) = copied.await {
        let _ = tokio::fs::remove_dir_all(&staged).await;
        return Err(e);
    }
    tokio::fs::rename(&staged, &layout.data_dir)
        .await
        .map_err(|e| {
            SnapshotError::io(format!(
                "the atomic switch of {} failed: {e}",
                layout.data_dir.display()
            ))
        })?;
    progress.on(Progress::Finished { ok: true });
    Ok(Restored {
        snapshot: snapshot.clone(),
        location: layout.data_dir.display().to_string(),
        activated: false,
        replaced_aside: aside_path,
    })
}

/// Reads the pid a `postmaster.pid`'s first line names, when the file parses.
async fn read_pid_first_line(pid_path: &Path) -> Option<u32> {
    let bytes = tokio::fs::read(pid_path).await.ok()?;
    let first = String::from_utf8_lossy(&bytes)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    first.parse().ok()
}

/// `true` when the pid names a process that exists on this machine. Read through
/// `/proc/<pid>` (`std::fs`, no unsafe: the workspace denies `unsafe` outside the
/// supervisor's sanctioned files); a missing /proc entry means the process is gone.
#[cfg(all(unix, target_os = "linux"))]
fn pid_alive_here(pid: u32) -> bool {
    std::fs::metadata(format!("/proc/{pid}")).is_ok()
}

/// Other Unix platforms without a procfs guarantee: no liveness probe is possible
/// without `unsafe`, so a parseable pid is treated as alive (the conservative read:
/// the operator stops the instance, then restores — the supervisor's own Windows
/// stance), never wrongly restored-over.
#[cfg(all(unix, not(target_os = "linux")))]
fn pid_alive_here(pid: u32) -> bool {
    pid != 0
}

/// Windows: no procfs; a stored pid is assumed alive, so the typed refusal fires.
#[cfg(windows)]
fn pid_alive_here(pid: u32) -> bool {
    pid != 0
}

/// Whether a directory holds any entry.
async fn has_entries(dir: &Path) -> bool {
    match tokio::fs::read_dir(dir).await {
        Ok(mut entries) => entries
            .next_entry()
            .await
            .map(|next| next.is_some())
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Renames a data directory aside to a clearly named neighbour; never dropped here.
async fn rename_dir_aside(dir: &Path, at_unix_ms: u64) -> SnapshotResult<String> {
    let aside = format!("{}.pre-restore-{at_unix_ms}", dir.display());
    tokio::fs::rename(dir, &aside)
        .await
        .map_err(|e| SnapshotError::io(format!("cannot move {} aside: {e}", dir.display())))?;
    Ok(aside)
}
