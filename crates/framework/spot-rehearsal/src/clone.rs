//! The clone: a scratch copy of the site a rehearsal may write to freely.
//!
//! A clone is a snapshot of the site restored into a scratch database. It is owned by
//! [`RehearsalClone`], an RAII guard that destroys the scratch and removes the snapshot directory
//! on **every** exit path — success, an early `?`, or a panic. [`obtain_clone`] is the one place
//! that talks to the [`SnapshotProvider`]; the rest of the crate only reads the [`CloneTarget`].
//!
//! The live site is never opened here: the snapshot is the only thing read from it, and the write
//! target is the scratch database the caller named.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use spot_core::SiteId;
use spot_snapshot::{
    Destination, DropOutcome, Progress, ProgressSink, RestoreOptions, RestoreTarget, ScratchMarker,
    SnapshotOptions, SnapshotProvider, SnapshotTarget, drop_scratch,
};

use crate::BoxFuture;
use crate::error::{Error, Result};

/// A scratch clone the rehearsal created and must destroy on every exit path.
///
/// The reaper is the backend-specific half (dropping the scratch database); the snapshot
/// directory is removed by [`RehearsalClone`] itself. `destroy` must be synchronous and must not
/// block on a runtime, because it runs from `Drop`.
pub trait Scratch: Send + Sync {
    /// A stable description of the scratch (never a secret), for the report.
    fn describe(&self) -> String;

    /// Tells the scratch which snapshot it is about to be restored from, so a backend that must
    /// present a marker to drop it (see [`SnapshotScratch`]) knows it. Called after the take and
    /// before the restore. The default does nothing.
    fn bind_snapshot(&self, _snapshot_id: &str) {}

    /// Destroys the scratch. A failure is recorded in the cleanup outcome, never panicked.
    ///
    /// This is the synchronous path `Drop` uses when nobody closed the clone; it must not block
    /// on the caller's runtime.
    fn destroy(&self) -> std::result::Result<(), String>;

    /// Destroys the scratch from async code. The default runs [`destroy`](Scratch::destroy).
    fn destroy_async(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(async move { self.destroy() })
    }
}

/// The real reaper: drops the scratch database through [`spot_snapshot::drop_scratch`], which
/// refuses anything that is not a marked scratch, the live database or a system database.
///
/// `connection` is a connection string to the server whose own database is the **live** one (so
/// that database is never dropped). The marker is bound by [`obtain_clone`] once the snapshot
/// exists; with no snapshot nothing was restored and there is nothing to drop. An instance
/// target is refused (`drop_scratch` drops databases).
pub struct SnapshotScratch {
    connection: String,
    target: RestoreTarget,
    marker: Mutex<Option<ScratchMarker>>,
}

impl SnapshotScratch {
    /// A reaper for the scratch at `target` on the server `connection` names.
    pub fn new(connection: impl Into<String>, target: RestoreTarget) -> SnapshotScratch {
        SnapshotScratch {
            connection: connection.into(),
            target,
            marker: Mutex::new(None),
        }
    }

    fn marker(&self) -> Option<ScratchMarker> {
        self.marker.lock().ok().and_then(|held| held.clone())
    }

    async fn drop_it(&self) -> std::result::Result<(), String> {
        let Some(marker) = self.marker() else {
            return Ok(());
        };
        match drop_scratch(&self.connection, &self.target, &marker).await {
            Ok(DropOutcome::Dropped | DropOutcome::NotFound) => Ok(()),
            // The code and the scratch's name only: the provider's message may quote a
            // connection string.
            Err(error) => Err(format!("{} ({})", error.code(), self.describe())),
        }
    }
}

impl Scratch for SnapshotScratch {
    fn describe(&self) -> String {
        format!("scratch {}", scratch_label(&self.target))
    }

    fn bind_snapshot(&self, snapshot_id: &str) {
        if let Ok(mut held) = self.marker.lock() {
            *held = Some(ScratchMarker::for_snapshot_id(snapshot_id));
        }
    }

    fn destroy(&self) -> std::result::Result<(), String> {
        // The `Drop` fallback: a private runtime on its own thread, so this works whether or not
        // the caller is inside a runtime and never blocks it.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| format!("no runtime to drop {}", self.describe()))?
                        .block_on(self.drop_it())
                })
                .join()
                .unwrap_or_else(|_| Err(format!("the drop of {} panicked", self.describe())))
        })
    }

    fn destroy_async(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(self.drop_it())
    }
}

/// What the engine and its collaborators use to address the clone.
///
/// `Debug` is implemented by hand so a restore target's password (in the instance variant) can
/// never reach a log through this type.
#[derive(Clone)]
pub struct CloneTarget {
    site: String,
    label: String,
    restore: RestoreTarget,
}

impl CloneTarget {
    /// A clone address for `site` at `restore`. Normally built by [`obtain_clone`]; a host that
    /// holds a clone it did not create here (for example a dry run) builds it directly.
    pub fn new(site: impl Into<String>, restore: RestoreTarget) -> CloneTarget {
        let label = scratch_label(&restore);
        CloneTarget {
            site: site.into(),
            label,
            restore,
        }
    }

    /// The site the clone belongs to.
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The scratch's label: a database name or a data-directory path. Never a secret.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The restore target the clone lives at.
    pub fn restore(&self) -> &RestoreTarget {
        &self.restore
    }
}

impl fmt::Debug for CloneTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.restore {
            RestoreTarget::Database { .. } => "database",
            RestoreTarget::Instance { .. } => "instance",
        };
        formatter
            .debug_struct("CloneTarget")
            .field("site", &self.site)
            .field("label", &self.label)
            .field("kind", &kind)
            .finish()
    }
}

/// How the clone was disposed of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupOutcome {
    /// The scratch was destroyed and the snapshot directory removed.
    Cleaned,
    /// Something could not be destroyed; the reason names what is left behind (never a secret).
    Failed {
        /// What was left behind and why.
        reason: String,
    },
}

impl CleanupOutcome {
    /// Whether the clone was fully cleaned up.
    pub fn cleaned(&self) -> bool {
        matches!(self, CleanupOutcome::Cleaned)
    }
}

/// Everything [`obtain_clone`] needs to make a clone.
pub struct CloneRequest {
    /// The live site to snapshot (read only).
    pub source: SnapshotTarget,
    /// Where the snapshot is written.
    pub destination: Destination,
    /// The scratch database the snapshot is restored into.
    pub scratch: RestoreTarget,
    /// Options for the take.
    pub snapshot_options: SnapshotOptions,
    /// Options for the restore.
    pub restore_options: RestoreOptions,
    /// How to destroy the scratch database. The caller supplies it because dropping a database is
    /// not a provider operation; the snapshot directory is removed without it.
    pub reaper: Option<Arc<dyn Scratch>>,
}

impl CloneRequest {
    /// A request with default take/restore options and no reaper.
    pub fn new(
        source: SnapshotTarget,
        destination: Destination,
        scratch: RestoreTarget,
    ) -> CloneRequest {
        CloneRequest {
            source,
            destination,
            scratch,
            snapshot_options: SnapshotOptions::default(),
            restore_options: RestoreOptions::default(),
            reaper: None,
        }
    }
}

/// A clone owned by the rehearsal. Dropping it destroys the clone.
pub struct RehearsalClone {
    target: CloneTarget,
    snapshot_id: String,
    snapshot_dir: PathBuf,
    reaper: Option<Arc<dyn Scratch>>,
    outcome: Option<CleanupOutcome>,
}

impl RehearsalClone {
    /// Builds the guard. The scratch is destroyed and the snapshot directory removed on `close`,
    /// which `Drop` calls if the caller has not.
    fn new(
        site: &SiteId,
        restore: RestoreTarget,
        snapshot_dir: PathBuf,
        reaper: Option<Arc<dyn Scratch>>,
    ) -> RehearsalClone {
        RehearsalClone {
            target: CloneTarget::new(site.to_string(), restore),
            snapshot_id: String::new(),
            snapshot_dir,
            reaper,
            outcome: None,
        }
    }

    /// The clone's address.
    pub fn target(&self) -> &CloneTarget {
        &self.target
    }

    /// The snapshot the clone was restored from (empty before the take completes).
    pub fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }

    /// The snapshot directory that is removed on cleanup.
    pub fn snapshot_dir(&self) -> &Path {
        &self.snapshot_dir
    }

    /// Destroys the clone and returns how it went. Idempotent: the first outcome is kept. This is
    /// the synchronous path `Drop` uses; code that can await calls
    /// [`close_async`](RehearsalClone::close_async) instead.
    pub fn close(&mut self) -> CleanupOutcome {
        if let Some(outcome) = &self.outcome {
            return outcome.clone();
        }
        let scratch = self.reaper.as_ref().map(|reaper| {
            reaper
                .destroy()
                .map_err(|reason| format!("scratch {}: {reason}", reaper.describe()))
        });
        self.finish(scratch)
    }

    /// [`close`](RehearsalClone::close) for async code: the scratch is dropped through the
    /// reaper's async path.
    pub async fn close_async(&mut self) -> CleanupOutcome {
        if let Some(outcome) = &self.outcome {
            return outcome.clone();
        }
        let scratch = match &self.reaper {
            Some(reaper) => Some(
                reaper
                    .destroy_async()
                    .await
                    .map_err(|reason| format!("scratch {}: {reason}", reaper.describe())),
            ),
            None => None,
        };
        self.finish(scratch)
    }

    /// Removes the snapshot directory and records the outcome.
    fn finish(&mut self, scratch: Option<std::result::Result<(), String>>) -> CleanupOutcome {
        let mut problems: Vec<String> = Vec::new();
        if let Some(Err(problem)) = scratch {
            problems.push(problem);
        }
        if self.snapshot_dir.exists()
            && let Err(error) = std::fs::remove_dir_all(&self.snapshot_dir)
        {
            problems.push(format!(
                "snapshot directory {}: {error}",
                self.snapshot_dir.display()
            ));
        }
        let outcome = if problems.is_empty() {
            CleanupOutcome::Cleaned
        } else {
            CleanupOutcome::Failed {
                reason: problems.join("; "),
            }
        };
        self.outcome = Some(outcome.clone());
        outcome
    }
}

impl Drop for RehearsalClone {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Obtains a clone: take a snapshot of `request.source`, restore it into `request.scratch`, and
/// return the guard that owns it.
///
/// # Errors
/// [`Error::SnapshotFailed`] when the take or the restore fails. The snapshot directory is removed
/// on the way out; the scratch is destroyed by the returned guard's `Drop`.
pub async fn obtain_clone(
    provider: &dyn SnapshotProvider,
    request: &CloneRequest,
    site: &SiteId,
    progress: Arc<dyn ProgressSink>,
) -> Result<RehearsalClone> {
    let mut clone = RehearsalClone::new(
        site,
        request.scratch.clone(),
        request.destination.dir.clone(),
        request.reaper.clone(),
    );
    let result = take_and_restore(provider, request, &mut clone, progress).await;
    match result {
        Ok(()) => Ok(clone),
        Err(error) => {
            // Cleaned here so a failed drop is not left to `Drop`; the cause stays the error.
            let _ = clone.close_async().await;
            Err(error)
        }
    }
}

async fn take_and_restore(
    provider: &dyn SnapshotProvider,
    request: &CloneRequest,
    clone: &mut RehearsalClone,
    progress: Arc<dyn ProgressSink>,
) -> Result<()> {
    let taken = provider
        .take(
            &request.source,
            &request.destination,
            &request.snapshot_options,
            progress.clone(),
        )
        .await
        .map_err(|error| Error::SnapshotFailed {
            reason: error.to_string(),
        })?;
    clone.snapshot_id = taken.snapshot.manifest().id.clone();
    if let Some(reaper) = &request.reaper {
        reaper.bind_snapshot(&clone.snapshot_id);
    }
    provider
        .restore(
            &taken.snapshot,
            &request.scratch,
            &request.restore_options,
            progress,
        )
        .await
        .map_err(|error| Error::SnapshotFailed {
            reason: error.to_string(),
        })?;
    Ok(())
}

/// The scratch's label from its restore target.
fn scratch_label(restore: &RestoreTarget) -> String {
    match restore {
        RestoreTarget::Database { database, .. } => database.clone(),
        RestoreTarget::Instance { layout, .. } => layout.data_dir.display().to_string(),
    }
}

/// A [`ProgressSink`] that discards events, for a rehearsal that reports nothing.
#[derive(Default)]
pub struct SilentProgress;

impl ProgressSink for SilentProgress {
    fn on(&self, _event: Progress) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingReaper {
        destroyed: Arc<AtomicUsize>,
    }

    impl Scratch for CountingReaper {
        fn describe(&self) -> String {
            "scratch test".to_owned()
        }

        fn destroy(&self) -> std::result::Result<(), String> {
            self.destroyed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn site() -> SiteId {
        SiteId::new("localhost").expect("site id")
    }

    fn database_target() -> RestoreTarget {
        RestoreTarget::Database {
            connection: "postgres://u:p@localhost/postgres".to_owned(),
            database: "spot_scratch".to_owned(),
        }
    }

    #[test]
    fn drop_destroys_the_scratch_once() {
        let destroyed = Arc::new(AtomicUsize::new(0));
        let reaper = Arc::new(CountingReaper {
            destroyed: destroyed.clone(),
        });
        {
            let mut clone = RehearsalClone::new(
                &site(),
                database_target(),
                PathBuf::from("/nonexistent/rehearsal-snapshot"),
                Some(reaper),
            );
            assert!(clone.close().cleaned());
            assert!(clone.close().cleaned());
        }
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn drop_runs_on_a_panic() {
        let destroyed = Arc::new(AtomicUsize::new(0));
        let reaper = Arc::new(CountingReaper {
            destroyed: destroyed.clone(),
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _clone = RehearsalClone::new(
                &site(),
                database_target(),
                PathBuf::from("/nonexistent/rehearsal-snapshot"),
                Some(reaper),
            );
            panic!("boom");
        }));
        assert!(result.is_err());
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
    }

    fn unreachable_scratch() -> SnapshotScratch {
        SnapshotScratch::new(
            "postgres://u:hunter2@127.0.0.1:1/live",
            RestoreTarget::Database {
                connection: "postgres://u:hunter2@127.0.0.1:1/live".to_owned(),
                database: "spot_scratch".to_owned(),
            },
        )
    }

    #[tokio::test]
    async fn snapshot_scratch_with_no_snapshot_has_nothing_to_drop() {
        assert_eq!(unreachable_scratch().destroy_async().await, Ok(()));
    }

    #[tokio::test]
    async fn snapshot_scratch_reports_a_failed_drop_without_the_password() {
        let scratch = unreachable_scratch();
        scratch.bind_snapshot("01890000-0000-7000-8000-000000000000");
        let reason = scratch
            .destroy_async()
            .await
            .expect_err("server unreachable");
        assert!(reason.contains("snapshot."), "{reason}");
        assert!(!reason.contains("hunter2"), "{reason}");
        // The `Drop` fallback fails the same way, on its own thread.
        let reason = scratch.destroy().expect_err("server unreachable");
        assert!(!reason.contains("hunter2"), "{reason}");
    }

    #[test]
    fn debug_never_shows_a_password() {
        let clone = RehearsalClone::new(
            &site(),
            RestoreTarget::Database {
                connection: "postgres://u:hunter2@localhost/postgres".to_owned(),
                database: "spot_scratch".to_owned(),
            },
            PathBuf::from("/nonexistent"),
            None,
        );
        let text = format!("{:?}", clone.target());
        assert!(!text.contains("hunter2"));
    }
}
