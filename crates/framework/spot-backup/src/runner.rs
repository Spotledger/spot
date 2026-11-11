//! The backup runner: status, produce, write, verify, book-keep and retain.
//!
//! One run is one job. It never reads the wall clock — `now` is an argument — and it
//! never counts a package as the last good backup until verification passed. A
//! removable target whose media is absent returns [`BackupRunOutcome::WaitingForMedia`]
//! and notifies the operator; it is not an error.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use spot_jobs::model::InsertResult;
use spot_snapshot::{
    Destination, Progress, ProgressSink, SnapshotOptions, SnapshotProvider, SnapshotTarget,
};
use time::OffsetDateTime;

use crate::error::{BackupError, Result};
use crate::manifest::BackupManifest;
use crate::model::{
    BackupJob, BackupRecord, BackupRequest, BackupRunOutcome, BackupRunReport, StoredBackup,
    TargetStatus, VerifyMode,
};
use crate::package::PackagedBackup;
use crate::retention::plan;
use crate::site::{AlertSink, HealthSink, InvariantRunner, MediaNotifier, ScratchSite};
use crate::target::BackupTarget;
use crate::verify::{VerifyReport, alert_for, condition_for, full, quick, require_scratch};

/// A boxed future for the producer port.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Produces the package a run writes (a snapshot provider plus any host preprocessing).
pub trait BackupProducer: Send + Sync {
    /// Produces the package for `request`.
    ///
    /// # Errors
    /// [`BackupError`] when the snapshot provider or packaging fails.
    fn produce<'a>(&'a self, request: &'a BackupRequest) -> BoxFuture<'a, Result<PackagedBackup>>;
}

/// The record of which backups are verified and which an open restore references.
///
/// A host's implementation must be durable: retention treats this ledger as the only
/// source of truth for "verified", because a manifest cannot prove its own verification.
pub trait BackupLedger: Send + Sync {
    /// Every record, newest first.
    fn records(&self) -> Vec<BackupRecord>;

    /// The newest verified record.
    fn last_verified(&self) -> Option<BackupRecord>;

    /// Inserts or replaces the record for its id.
    fn upsert(&self, record: BackupRecord);

    /// Removes a record.
    fn remove(&self, id: &str);
}

/// An in-memory ledger for hosts whose durable ledger is not wired yet and for tests.
#[derive(Debug, Default)]
pub struct InMemoryLedger {
    records: Mutex<Vec<BackupRecord>>,
}

impl InMemoryLedger {
    /// An empty ledger.
    pub fn new() -> InMemoryLedger {
        InMemoryLedger::default()
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, Vec<BackupRecord>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl BackupLedger for InMemoryLedger {
    fn records(&self) -> Vec<BackupRecord> {
        let mut records = self.guard().clone();
        records.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.sequence.cmp(&left.sequence))
        });
        records
    }

    fn last_verified(&self) -> Option<BackupRecord> {
        self.records().into_iter().find(|record| record.verified)
    }

    fn upsert(&self, record: BackupRecord) {
        let mut records = self.guard();
        records.retain(|existing| existing.id != record.id);
        records.push(record);
    }

    fn remove(&self, id: &str) {
        self.guard().retain(|existing| existing.id != id);
    }
}

/// Everything one run needs. Optional ports are required only for `Full` verification.
pub struct BackupRunner<'a> {
    /// The destination to write to.
    pub target: &'a dyn BackupTarget,
    /// The package producer.
    pub producer: &'a dyn BackupProducer,
    /// Which backups passed verification.
    pub ledger: &'a dyn BackupLedger,
    /// Where failures raise health conditions.
    pub health: &'a dyn HealthSink,
    /// Where failures send alerts.
    pub alerts: &'a dyn AlertSink,
    /// Where absent removable media is announced.
    pub notifier: &'a dyn MediaNotifier,
    /// The scratch site for `Full` verification.
    pub scratch: Option<&'a dyn ScratchSite>,
    /// The invariant registry for `Full` verification.
    pub invariants: Option<&'a dyn InvariantRunner>,
}

impl std::fmt::Debug for BackupRunner<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackupRunner")
            .field("target", &self.target.describe())
            .finish_non_exhaustive()
    }
}

impl BackupRunner<'_> {
    /// Runs `job` at `now`.
    ///
    /// # Errors
    /// [`BackupError::TargetUnavailable`] for a misconfigured target, or any
    /// production/write/verification error. Absent removable media is not an error.
    pub async fn run(&self, job: &BackupJob, now: OffsetDateTime) -> Result<BackupRunReport> {
        match self.target.status() {
            TargetStatus::Ready => {}
            TargetStatus::WaitingForMedia { volume } => {
                self.notifier.waiting(&volume);
                return Ok(BackupRunReport {
                    outcome: BackupRunOutcome::WaitingForMedia { volume },
                    deleted: Vec::new(),
                });
            }
            TargetStatus::Unavailable { reason } => {
                return Err(BackupError::TargetUnavailable {
                    target: self.target.describe(),
                    reason,
                });
            }
        }

        // A target folder or bucket may hold several tenants' backups; this job only
        // ever sees, chains from and (below) retains its own tenant's packages.
        let existing: Vec<StoredBackup> = self
            .target
            .list()?
            .into_iter()
            .filter(|backup| backup.manifest.tenant == job.tenant)
            .collect();
        let chain_latest = existing
            .iter()
            .filter(|backup| backup.manifest.chain_id == job.chain_id)
            .max_by_key(|backup| backup.manifest.chain_sequence);
        let sequence = chain_latest
            .map(|backup| backup.manifest.chain_sequence)
            .unwrap_or(0)
            .saturating_add(1);
        let parent_digest = chain_latest.map(|backup| backup.manifest.digest.clone());
        let id = format!(
            "{}-{}-{sequence}",
            sanitize(job.tenant.as_str()),
            sanitize(&job.chain_id)
        );
        let request = BackupRequest {
            job: BackupJob {
                parent_digest,
                sequence,
                ..job.clone()
            },
            now,
            id: id.clone(),
            database: job.tenant.clone(),
        };

        let packaged = self.producer.produce(&request).await?;

        // Verify before publishing: a target is handed bytes only once the package
        // passed its quick checks. Full verification runs against the same bytes.
        let verification = match job.verify {
            VerifyMode::Quick => quick(&packaged.package),
            VerifyMode::Full => {
                let (scratch, invariants) = require_scratch(self.scratch, self.invariants)?;
                full(&packaged.package, scratch, invariants).await?
            }
        };

        // A package that fails its cheap checks is not published at all: there is no
        // value in keeping bytes that cannot describe themselves. A full-verification
        // failure is published for diagnosis but never counted as the last good backup.
        if !verification.ok && job.verify == VerifyMode::Quick {
            self.record_failure(&packaged.manifest, now);
            self.health.raise(condition_for(&verification, &id));
            self.alerts.alert(alert_for(&verification, &id));
            return Err(BackupError::VerifyFailed {
                reason: verification
                    .failure
                    .clone()
                    .unwrap_or_else(|| "verification failed".to_owned()),
            });
        }

        let (stream_bytes, stream_digest) = packaged.package.stream_facts()?;
        let stored = match self.target.put(
            &id,
            packaged.package.reader()?,
            stream_bytes,
            &stream_digest,
        ) {
            Ok(stored) => stored,
            Err(error) => {
                self.record_failure(&packaged.manifest, now);
                return Err(error);
            }
        };

        let record = BackupRecord {
            id: stored.id.clone(),
            created_at: packaged.manifest.created_at()?,
            verified: verification.ok,
            sequence,
            open_restore: false,
        };
        self.ledger.upsert(record);
        if !verification.ok {
            self.raise_verification_failure(&stored, &verification);
            let invariants_failed = verification
                .invariants
                .as_ref()
                .is_some_and(|invariants| !invariants.ok);
            return Err(if invariants_failed {
                BackupError::InvariantsFailed {
                    violations: verification
                        .failure
                        .clone()
                        .unwrap_or_else(|| "invariants failed".to_owned()),
                }
            } else {
                BackupError::VerifyFailed {
                    reason: verification
                        .failure
                        .clone()
                        .unwrap_or_else(|| "verification failed".to_owned()),
                }
            });
        }

        let deleted = self.apply_retention(job, now)?;
        Ok(BackupRunReport {
            outcome: BackupRunOutcome::Created {
                backup: Box::new(stored),
                verification: Box::new(verification),
            },
            deleted,
        })
    }

    fn record_failure(&self, manifest: &BackupManifest, now: OffsetDateTime) {
        let record = BackupRecord {
            id: manifest.id.clone(),
            created_at: manifest.created_at().unwrap_or(now),
            verified: false,
            sequence: manifest.chain_sequence,
            open_restore: false,
        };
        self.ledger.upsert(record);
    }

    fn raise_verification_failure(&self, backup: &StoredBackup, report: &VerifyReport) {
        self.health.raise(condition_for(report, &backup.id));
        self.alerts.alert(alert_for(report, &backup.id));
    }

    fn apply_retention(&self, job: &BackupJob, now: OffsetDateTime) -> Result<Vec<String>> {
        let mut records: Vec<BackupRecord> = Vec::new();
        let ledger_records = self.ledger.records();
        for backup in self.target.list()? {
            if backup.manifest.tenant != job.tenant {
                continue;
            }
            let ledger = ledger_records.iter().find(|record| record.id == backup.id);
            let verified = ledger.is_some_and(|record| record.verified);
            let open_restore = ledger.is_some_and(|record| record.open_restore);
            records.push(BackupRecord::from_manifest(
                &backup.manifest,
                verified,
                open_restore,
            )?);
        }
        // A backup this run produced is verified (we only get here after verification).
        let plan = plan(&job.retention, &records, now)?;
        let mut deleted = Vec::new();
        for id in plan.delete {
            // Never delete something the ledger says is verified and an open restore
            // references; the planner already protects it, but the target call is the
            // last line of defence.
            let open = self
                .ledger
                .records()
                .iter()
                .any(|record| record.id == id && record.open_restore);
            if open {
                continue;
            }
            self.target.delete(&id)?;
            self.ledger.remove(&id);
            deleted.push(id);
        }
        Ok(deleted)
    }
}

/// Enqueues one already-claimed job result. The runner's caller completes the job with
/// this outcome; a `WaitingForMedia` result leaves the job pending rather than failed.
pub fn complete_result(report: &BackupRunReport) -> &'static str {
    match report.outcome {
        BackupRunOutcome::Created { .. } => "succeeded",
        BackupRunOutcome::WaitingForMedia { .. } => "waiting_for_media",
    }
}

/// The job-store insert result's job id, when it inserted.
pub fn inserted_id(result: &InsertResult) -> Option<spot_jobs::JobId> {
    match result {
        InsertResult::Inserted(id) => Some(*id),
        InsertResult::Duplicate(_) => None,
    }
}

fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            out.push(byte as char);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push_str("backup");
    }
    out
}

/// A snapshot provider behind [`BackupProducer`]: take into a temporary directory, then
/// package that directory.
pub struct SnapshotProducer {
    provider: Arc<dyn SnapshotProvider>,
    target: SnapshotTarget,
    work_dir: PathBuf,
    platform_version: String,
    schema_version: u64,
    app_set: Vec<String>,
}

impl SnapshotProducer {
    /// A producer over `provider` and `target`, writing temporary takes under
    /// `work_dir`.
    pub fn new(
        provider: Arc<dyn SnapshotProvider>,
        target: SnapshotTarget,
        work_dir: impl Into<PathBuf>,
        schema_version: u64,
        mut app_set: Vec<String>,
    ) -> SnapshotProducer {
        app_set.sort();
        app_set.dedup();
        SnapshotProducer {
            provider,
            target,
            work_dir: work_dir.into(),
            platform_version: env!("CARGO_PKG_VERSION").to_owned(),
            schema_version,
            app_set,
        }
    }
}

struct SilentProgress;

impl ProgressSink for SilentProgress {
    fn on(&self, _event: Progress) {}
}

impl BackupProducer for SnapshotProducer {
    fn produce<'a>(&'a self, request: &'a BackupRequest) -> BoxFuture<'a, Result<PackagedBackup>> {
        Box::pin(async move {
            let sequence = next_temp_sequence();
            let take_dir = self
                .work_dir
                .join(format!(".take-{}-{sequence}", std::process::id()));
            let outcome = self
                .provider
                .take(
                    &self.target,
                    &Destination {
                        dir: take_dir.clone(),
                    },
                    &SnapshotOptions::default(),
                    Arc::new(SilentProgress),
                )
                .await
                .map_err(BackupError::from)?;
            let manifest = BackupManifest {
                format: crate::manifest::MANIFEST_FORMAT.to_owned(),
                id: request.id.clone(),
                tenant: request.database.clone(),
                created_at: BackupManifest::format_instant(request.now),
                platform_version: self.platform_version.clone(),
                snapshot_provider_version: outcome.snapshot.manifest().provider_version.clone(),
                pg_major: outcome.snapshot.manifest().pg_major,
                schema_version: self.schema_version,
                app_set: self.app_set.clone(),
                digest: "0".repeat(64),
                size: 0,
                file_count: 0,
                chain_id: request.job.chain_id.clone(),
                chain_sequence: request.job.sequence,
                parent_digest: request.job.parent_digest.clone(),
                verify: request.job.verify,
                target: request.job.target.to_string(),
                encryption: request.job.encrypt.clone(),
            };
            let packaged =
                crate::package::build_from_dir(manifest, outcome.snapshot.dir(), &self.work_dir)?;
            let _ = tokio::fs::remove_dir_all(&take_dir).await;
            Ok(packaged)
        })
    }
}

fn next_temp_sequence() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
