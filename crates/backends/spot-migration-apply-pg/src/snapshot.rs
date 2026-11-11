//! The snapshot port over `spot-snapshot`, with take and restore timing capture.
//!
//! ADR-22 restores the whole site from a snapshot when a run fails after apply, before users
//! return. [`PgSnapshotPort`] is the engine's [`SnapshotPort`] seam over a
//! [`spot_snapshot::SnapshotProvider`]:
//!
//! - `take` calls the provider, records the manifest's id in
//!   `spot_migration_control.snapshot` (so a restart can resolve it) and records the take's
//!   duration;
//! - `exists` answers from that index and re-verifies the snapshot's manifest;
//! - `restore` opens the manifest, restores into the configured target (a database or a
//!   stopped instance) and records the restore's duration.
//!
//! The provider decides how the data moves; this crate never copies tenant data itself. A
//! logical dump provider (`PgDumpProvider`) and a stopped-instance provider
//! (`StoppedInstanceProvider`) both plug in unchanged.
//!
//! # Durations are integers
//!
//! Take and restore times come from the injected [`spot_core::Clock`]'s monotonic reading and
//! are recorded as microseconds in `spot_migration_control.timing`. The tests print them for
//! the 100-branch measurement WP-MIG-05 asks for.

use std::sync::Arc;

use spot_core::{Clock, SiteId};
use spot_migration_apply::snapshot::{RestoreOutcome, SnapshotId, SnapshotPort};
use spot_migration_apply::{BoxFuture, Error as ApplyError, Result as ApplyResult};
use spot_snapshot::{
    Destination, RestoreOptions, RestoreTarget, Snapshot, SnapshotOptions, SnapshotProvider,
    SnapshotTarget,
};

use crate::error::{Error, Result};
use crate::schema::{PgApplySchema, client, table};
use crate::timings::{TimingPhase, TimingRecord, TimingSink};

/// A snapshot progress sink that ignores every event: the engine reports its own progress.
struct SilentSnapshotProgress;

impl spot_snapshot::ProgressSink for SilentSnapshotProgress {
    fn on(&self, _event: spot_snapshot::Progress) {}
}

/// The engine's snapshot port over one `spot-snapshot` provider.
pub struct PgSnapshotPort {
    schema: PgApplySchema,
    site: SiteId,
    provider: Arc<dyn SnapshotProvider>,
    target: SnapshotTarget,
    restore_target: RestoreTarget,
    destination: std::path::PathBuf,
    snapshot_options: SnapshotOptions,
    restore_options: RestoreOptions,
    clock: Arc<dyn Clock>,
    timings: Arc<dyn TimingSink>,
    next: std::sync::atomic::AtomicU64,
}

impl PgSnapshotPort {
    /// A port for `site` that takes from `target` into `destination` and restores into
    /// `restore_target`.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        schema: PgApplySchema,
        site: SiteId,
        provider: Arc<dyn SnapshotProvider>,
        target: SnapshotTarget,
        restore_target: RestoreTarget,
        destination: impl Into<std::path::PathBuf>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            schema,
            site,
            provider,
            target,
            restore_target,
            destination: destination.into(),
            snapshot_options: SnapshotOptions::default(),
            restore_options: RestoreOptions::default(),
            clock,
            timings: Arc::new(crate::timings::NoTimings),
            next: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// The same port with different take options.
    #[must_use]
    pub fn with_snapshot_options(mut self, options: SnapshotOptions) -> Self {
        self.snapshot_options = options;
        self
    }

    /// The same port with different restore options.
    #[must_use]
    pub fn with_restore_options(mut self, options: RestoreOptions) -> Self {
        self.restore_options = options;
        self
    }

    /// The same port recording its take and restore timings through `timings`.
    #[must_use]
    pub fn with_timings(mut self, timings: Arc<dyn TimingSink>) -> Self {
        self.timings = timings;
        self
    }

    /// The site this port snapshots.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The destination root the next take is written under.
    #[must_use]
    pub fn destination(&self) -> &std::path::Path {
        &self.destination
    }

    /// The directory of a snapshot this port took, when it knows it.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the index cannot be read.
    pub async fn snapshot_dir(&self, id: &SnapshotId) -> Result<Option<std::path::PathBuf>> {
        let client = client(&self.schema_pool(), "reading the snapshot index").await?;
        let row = client
            .query_opt(
                &format!(
                    "SELECT dir FROM {} WHERE site = $1 AND snapshot_id = $2",
                    table("snapshot")
                ),
                &[&self.site.as_str(), &id.as_str()],
            )
            .await
            .map_err(|error| Error::database("reading the snapshot index", &error))?;
        Ok(row.map(|row| std::path::PathBuf::from(row.get::<_, String>(0))))
    }

    /// The take's destination for `label`: a fresh directory under the root, never an
    /// existing snapshot's.
    fn destination_for(&self, label: &str) -> std::path::PathBuf {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let safe: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        self.destination
            .join(format!("{}-{n:04}-{safe}", self.site))
    }

    fn now_micros(&self) -> i64 {
        i64::try_from(self.clock.monotonic().as_micros()).unwrap_or(i64::MAX)
    }

    fn record(
        &self,
        phase: TimingPhase,
        elapsed: i64,
        snapshot: &SnapshotId,
        at: i64,
    ) -> Result<()> {
        self.timings
            .record(TimingRecord::new(phase, elapsed, snapshot.as_str(), at))
            .map_err(|error| Error::TimingFailed {
                message: error.to_string(),
            })
    }

    /// The pool behind the schema manager, for the index reads.
    fn schema_pool(&self) -> deadpool_postgres::Pool {
        self.schema.pool()
    }
}

impl SnapshotPort for PgSnapshotPort {
    fn take<'a>(&'a self, label: &'a str, now: i64) -> BoxFuture<'a, ApplyResult<SnapshotId>> {
        Box::pin(async move {
            let destination = self.destination_for(label);
            let started = self.now_micros();
            let outcome = self
                .provider
                .take(
                    &self.target,
                    &Destination {
                        dir: destination.clone(),
                    },
                    &self.snapshot_options,
                    Arc::new(SilentSnapshotProgress),
                )
                .await
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: Error::SnapshotFailed {
                        operation: "take".to_owned(),
                        message: error.to_string(),
                    }
                    .to_string(),
                })?;
            let elapsed = self.now_micros().saturating_sub(started);
            let id = SnapshotId::new(outcome.snapshot.manifest().id.clone())?;
            let client = client(&self.schema_pool(), "recording the snapshot")
                .await
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: error.to_string(),
                })?;
            client
                .execute(
                    &format!(
                        "INSERT INTO {} (site, snapshot_id, dir, label, taken_at) \
                         VALUES ($1, $2, $3, $4, $5) \
                         ON CONFLICT (site, snapshot_id) DO UPDATE SET \
                             dir = EXCLUDED.dir, label = EXCLUDED.label",
                        table("snapshot")
                    ),
                    &[
                        &self.site.as_str(),
                        &id.as_str(),
                        &destination.display().to_string(),
                        &label,
                        &now,
                    ],
                )
                .await
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: Error::database("recording the snapshot", &error).to_string(),
                })?;
            self.record(TimingPhase::Snapshot, elapsed, &id, now)
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: error.to_string(),
                })?;
            Ok(id)
        })
    }

    fn exists<'a>(&'a self, id: &'a SnapshotId) -> BoxFuture<'a, ApplyResult<bool>> {
        Box::pin(async move {
            let Some(dir) =
                self.snapshot_dir(id)
                    .await
                    .map_err(|error| ApplyError::SnapshotFailed {
                        message: error.to_string(),
                    })?
            else {
                return Ok(false);
            };
            match Snapshot::open(&dir).await {
                Ok(snapshot) => Ok(snapshot.manifest().id == id.as_str()),
                Err(_) => Ok(false),
            }
        })
    }

    fn restore<'a>(
        &'a self,
        id: &'a SnapshotId,
        now: i64,
    ) -> BoxFuture<'a, ApplyResult<RestoreOutcome>> {
        Box::pin(async move {
            let dir = self
                .snapshot_dir(id)
                .await
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: error.to_string(),
                })?
                .ok_or_else(|| ApplyError::SnapshotFailed {
                    message: Error::SnapshotUnknown {
                        snapshot: id.to_string(),
                    }
                    .to_string(),
                })?;
            let snapshot =
                Snapshot::open(&dir)
                    .await
                    .map_err(|error| ApplyError::SnapshotFailed {
                        message: Error::SnapshotFailed {
                            operation: "open".to_owned(),
                            message: error.to_string(),
                        }
                        .to_string(),
                    })?;
            let started = self.now_micros();
            let restored = self
                .provider
                .restore(
                    &snapshot,
                    &self.restore_target,
                    &self.restore_options,
                    Arc::new(SilentSnapshotProgress),
                )
                .await
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: Error::SnapshotFailed {
                        operation: "restore".to_owned(),
                        message: error.to_string(),
                    }
                    .to_string(),
                })?;
            let elapsed = self.now_micros().saturating_sub(started);
            self.record(TimingPhase::Restore, elapsed, id, now)
                .map_err(|error| ApplyError::SnapshotFailed {
                    message: error.to_string(),
                })?;
            Ok(RestoreOutcome {
                snapshot: id.clone(),
                location: restored.location,
                at: now,
            })
        })
    }
}

impl std::fmt::Debug for PgSnapshotPort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgSnapshotPort")
            .field("site", &self.site)
            .field("provider", &self.provider.name())
            .field("destination", &self.destination)
            .finish_non_exhaustive()
    }
}
