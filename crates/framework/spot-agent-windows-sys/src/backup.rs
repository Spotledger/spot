//! Coordination of a Volume Shadow Copy with PostgreSQL backup mode, and the RAII guard
//! that guarantees backup mode is left on **every** path.
//!
//! The sequence the brief fixes is: `pg_backup_start`, VSS create, VSS expose read-only,
//! `pg_backup_stop` with the label `pg_backup_start` returned. A shadow copy of a running
//! PostgreSQL data directory is only recoverable inside that window, so backup mode must
//! never be left on: [`PgBackupGuard`] stops it from `Drop` unless it was stopped
//! explicitly, and [`SnapshotCoordinator::run`] deletes any shadow copy it created before
//! it returns an error.
//!
//! The database side is behind [`PgBackupPort`] so this crate holds no connection and the
//! tests inject a failure at every step. A real port is built on the `spot-postgres`
//! connection facade by the host.

use std::path::{Path, PathBuf};

use crate::error::{WindowsSysError, WindowsSysResult};
use crate::vss::{ShadowCopy, ShadowCopyId, VolumeSet, VssApi, WriterSet};

/// The label `pg_backup_start` returned; `pg_backup_stop` must be given it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupLabel(String);

impl BackupLabel {
    /// Wraps a label as the database reported it.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        BackupLabel(text.into())
    }

    /// The label as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The transaction horizon a backup start reports, recorded in the manifest's consistency
/// statement. Both are integers; no floats anywhere near a size or a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackupHorizon {
    /// The lowest transaction id still visible.
    pub xmin: u64,
    /// The epoch of `xmin`, as the database reports it.
    pub epoch: u64,
}

/// What one `backup_start` returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupSession {
    /// The label to hand back to `backup_stop`.
    pub label: BackupLabel,
    /// The horizon at which the copy is consistent.
    pub horizon: BackupHorizon,
}

/// PostgreSQL backup mode, as the coordinator drives it. The real implementation lives on
/// the host, over the `spot-postgres` connection facade; the tests use [`FakePgBackup`].
pub trait PgBackupPort: Send + Sync {
    /// Enters backup mode and returns the label and the horizon.
    ///
    /// # Errors
    /// [`WindowsSysError::BackupStartFailed`], or [`WindowsSysError::BackupAlreadyActive`]
    /// when backup mode is already on.
    fn backup_start(&self) -> WindowsSysResult<BackupSession>;

    /// Leaves backup mode, naming the label `backup_start` returned.
    ///
    /// # Errors
    /// [`WindowsSysError::BackupStopFailed`], or [`WindowsSysError::BackupNotActive`] when
    /// no backup mode is on.
    fn backup_stop(&self, label: &BackupLabel) -> WindowsSysResult<()>;

    /// Whether backup mode is currently on.
    ///
    /// # Errors
    /// [`WindowsSysError::Io`] when the state cannot be read.
    fn in_backup_mode(&self) -> WindowsSysResult<bool>;
}

/// The RAII guard that leaves backup mode. [`PgBackupGuard::stop`] leaves it explicitly and
/// records the result; if the guard is dropped first (an error path), `Drop` leaves it and
/// ignores a second failure — the important thing is that it is attempted.
pub struct PgBackupGuard<'a> {
    port: &'a dyn PgBackupPort,
    label: Option<BackupLabel>,
}

impl<'a> PgBackupGuard<'a> {
    /// A guard that will leave backup mode for `session` unless stopped first.
    #[must_use]
    pub fn new(port: &'a dyn PgBackupPort, session: BackupSession) -> Self {
        PgBackupGuard {
            port,
            label: Some(session.label),
        }
    }

    /// Leaves backup mode and returns the database's result. On success the guard's `Drop`
    /// becomes a no-op; on failure the label stays armed, so `Drop` retries the stop once
    /// (the RAII backstop for exactly this error path).
    ///
    /// # Errors
    /// [`WindowsSysError::BackupStopFailed`] from the port.
    pub fn stop(&mut self) -> WindowsSysResult<()> {
        match &self.label {
            Some(label) => {
                self.port.backup_stop(label)?;
                self.label = None;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Whether backup mode is still the guard's responsibility.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.label.is_some()
    }
}

impl Drop for PgBackupGuard<'_> {
    fn drop(&mut self) {
        if let Some(label) = self.label.take() {
            let _ = self.port.backup_stop(&label);
        }
    }
}

/// A live coordinated snapshot: the shadow copy is created and exposed, and backup mode is
/// already left. Dropping it unexposes and deletes the shadow copy; call
/// [`CoordinatedSnapshot::finish`] to see the result of that cleanup.
pub struct CoordinatedSnapshot<'a> {
    vss: &'a dyn VssApi,
    shadow: ShadowCopy,
    exposed: PathBuf,
    horizon: BackupHorizon,
    finished: bool,
}

impl<'a> CoordinatedSnapshot<'a> {
    /// The shadow copy.
    #[must_use]
    pub fn shadow(&self) -> &ShadowCopy {
        &self.shadow
    }

    /// The read-only path the data can be read through.
    #[must_use]
    pub fn exposed_path(&self) -> &Path {
        &self.exposed
    }

    /// The transaction horizon reported when backup mode was entered.
    #[must_use]
    pub fn horizon(&self) -> BackupHorizon {
        self.horizon
    }

    /// Unexposes and deletes the shadow copy, reporting the first failure. Idempotent.
    ///
    /// # Errors
    /// [`WindowsSysError`] from [`VssApi::unexpose`] or [`VssApi::delete`].
    pub fn finish(mut self) -> WindowsSysResult<()> {
        let result = self.cleanup();
        self.finished = true;
        result
    }

    fn cleanup(&mut self) -> WindowsSysResult<()> {
        let unexposed = self.vss.unexpose(&self.shadow.id);
        let deleted = self.vss.delete(&self.shadow.id);
        unexposed.and(deleted)
    }
}

impl Drop for CoordinatedSnapshot<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.cleanup();
        }
    }
}

impl std::fmt::Debug for CoordinatedSnapshot<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CoordinatedSnapshot")
            .field("shadow", &self.shadow.id)
            .field("exposed", &self.exposed)
            .field("finished", &self.finished)
            .finish()
    }
}

/// Runs the fixed sequence over one [`VssApi`] and one [`PgBackupPort`].
pub struct SnapshotCoordinator<'a> {
    vss: &'a dyn VssApi,
    backup: &'a dyn PgBackupPort,
}

impl<'a> SnapshotCoordinator<'a> {
    /// A coordinator over `vss` and `backup`.
    #[must_use]
    pub fn new(vss: &'a dyn VssApi, backup: &'a dyn PgBackupPort) -> Self {
        SnapshotCoordinator { vss, backup }
    }

    /// Runs `pg_backup_start`, VSS create, expose, `pg_backup_stop`, in that order.
    ///
    /// On any failure after the shadow copy exists it is deleted before the error is
    /// returned, and backup mode is always left (the guard's `Drop`). `source` is the
    /// directory whose data the caller will read through the exposed path; `at_unix_ms` is
    /// the creation instant, supplied by the caller's clock.
    ///
    /// # Errors
    /// The step's own [`WindowsSysError`].
    pub fn run(
        &self,
        volumes: &VolumeSet,
        writers: &WriterSet,
        source: &Path,
        at_unix_ms: u64,
    ) -> WindowsSysResult<CoordinatedSnapshot<'a>> {
        let session = self.backup.backup_start()?;
        let horizon = session.horizon;
        let mut guard = PgBackupGuard::new(self.backup, session);

        // If the create fails, `?` returns here and `guard` drops, leaving backup mode.
        let shadow = self.vss.create(volumes, writers, at_unix_ms)?;

        let exposed = match self.vss.expose_read_only(&shadow.id, source) {
            Ok(path) => path,
            Err(error) => {
                let _ = self.vss.delete(&shadow.id);
                // `guard` drops here and leaves backup mode.
                return Err(error);
            }
        };

        if let Err(error) = guard.stop() {
            let _ = self.vss.delete(&shadow.id);
            return Err(error);
        }

        Ok(CoordinatedSnapshot {
            vss: self.vss,
            shadow,
            exposed,
            horizon,
            finished: false,
        })
    }
}

/// The in-memory PostgreSQL backup port used by every test. It records the calls it
/// receives, tracks whether backup mode is on, and can be told to fail at one step.
#[derive(Debug)]
pub struct FakePgBackup {
    state: std::sync::Mutex<FakePgState>,
}

#[derive(Debug)]
struct FakePgState {
    active_label: Option<String>,
    starts: u64,
    stops: u64,
    fail_start: bool,
    fail_stop: bool,
    horizon: BackupHorizon,
}

/// One step of the backup port, for failure injection in tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupStep {
    /// [`PgBackupPort::backup_start`].
    Start,
    /// [`PgBackupPort::backup_stop`].
    Stop,
}

impl FakePgBackup {
    /// A fake that reports the given horizon.
    #[must_use]
    pub fn new(horizon: BackupHorizon) -> Self {
        FakePgBackup {
            state: std::sync::Mutex::new(FakePgState {
                active_label: None,
                starts: 0,
                stops: 0,
                fail_start: false,
                fail_stop: false,
                horizon,
            }),
        }
    }

    /// Makes the named step fail once the fake is used.
    pub fn fail_at(&self, step: BackupStep) {
        let mut state = self.lock();
        match step {
            BackupStep::Start => state.fail_start = true,
            BackupStep::Stop => state.fail_stop = true,
        }
    }

    /// How many times `backup_start` was called.
    #[must_use]
    pub fn starts(&self) -> u64 {
        self.lock().starts
    }

    /// How many times `backup_stop` was called.
    #[must_use]
    pub fn stops(&self) -> u64 {
        self.lock().stops
    }

    /// Whether backup mode is currently on.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.lock().active_label.is_some()
    }

    /// The active label, when backup mode is on.
    #[must_use]
    pub fn active_label(&self) -> Option<String> {
        self.lock().active_label.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakePgState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl PgBackupPort for FakePgBackup {
    fn backup_start(&self) -> WindowsSysResult<BackupSession> {
        let mut state = self.lock();
        if state.fail_start {
            state.fail_start = false;
            return Err(WindowsSysError::BackupStartFailed {
                reason: "the fake was told to fail at start".to_owned(),
            });
        }
        if let Some(label) = &state.active_label {
            return Err(WindowsSysError::BackupAlreadyActive {
                label: label.clone(),
            });
        }
        state.starts += 1;
        let label = format!("pg-backup-{}", state.starts);
        state.active_label = Some(label.clone());
        let horizon = state.horizon;
        Ok(BackupSession {
            label: BackupLabel::new(label),
            horizon,
        })
    }

    fn backup_stop(&self, label: &BackupLabel) -> WindowsSysResult<()> {
        let mut state = self.lock();
        state.stops += 1;
        if state.fail_stop {
            state.fail_stop = false;
            return Err(WindowsSysError::BackupStopFailed {
                label: label.as_str().to_owned(),
                reason: "the fake was told to fail at stop".to_owned(),
            });
        }
        match &state.active_label {
            Some(active) if active == label.as_str() => {
                state.active_label = None;
                Ok(())
            }
            Some(active) => Err(WindowsSysError::BackupStopFailed {
                label: label.as_str().to_owned(),
                reason: format!("the active backup mode is {active}"),
            }),
            None => Err(WindowsSysError::BackupNotActive),
        }
    }

    fn in_backup_mode(&self) -> WindowsSysResult<bool> {
        Ok(self.lock().active_label.is_some())
    }
}

/// Deletes one shadow copy by id, used by callers that track ids themselves.
///
/// # Errors
/// [`WindowsSysError`] from [`VssApi::delete`].
pub fn delete_shadow(vss: &dyn VssApi, id: &ShadowCopyId) -> WindowsSysResult<()> {
    vss.delete(id)
}
