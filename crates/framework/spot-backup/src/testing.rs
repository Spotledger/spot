//! Deterministic doubles for the ports and for a restore target.
//!
//! These are part of the public API (not behind a test feature) so a host can start
//! wiring the backup UI before every platform adapter exists, and so the crate's tests
//! and later packages share one definition of each fake.

use std::sync::{Mutex, PoisonError};

use crate::error::{BackupError, Result};
use crate::manifest::{BackupManifest, MANIFEST_FORMAT};
use crate::model::{BackupJob, BackupRequest, VerifyMode};
use crate::package::{PackageEntry, PackageFile, PackagedBackup};
use crate::runner::{BackupProducer, BoxFuture};
use crate::site::{
    AlertSink, BackupAlert, HealthCondition, HealthSink, InvariantReport, InvariantRunner,
    MediaNotifier, MigrationApplied, RestoredSite, SchemaCheck, ServiceGate,
};

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A health sink that records every condition.
#[derive(Debug, Default)]
pub struct RecordingHealth {
    conditions: Mutex<Vec<HealthCondition>>,
}

impl RecordingHealth {
    /// A sink with nothing recorded.
    pub fn new() -> RecordingHealth {
        RecordingHealth::default()
    }

    /// The recorded conditions.
    pub fn conditions(&self) -> Vec<HealthCondition> {
        lock(&self.conditions).clone()
    }
}

impl HealthSink for RecordingHealth {
    fn raise(&self, condition: HealthCondition) {
        lock(&self.conditions).push(condition);
    }
}

/// An alert sink that records every alert.
#[derive(Debug, Default)]
pub struct RecordingAlerts {
    alerts: Mutex<Vec<BackupAlert>>,
}

impl RecordingAlerts {
    /// A sink with nothing recorded.
    pub fn new() -> RecordingAlerts {
        RecordingAlerts::default()
    }

    /// The recorded alerts.
    pub fn alerts(&self) -> Vec<BackupAlert> {
        lock(&self.alerts).clone()
    }
}

impl AlertSink for RecordingAlerts {
    fn alert(&self, alert: BackupAlert) {
        lock(&self.alerts).push(alert);
    }
}

/// A media notifier that records the volumes it was told about.
#[derive(Debug, Default)]
pub struct RecordingMedia {
    waiting: Mutex<Vec<String>>,
}

impl RecordingMedia {
    /// A notifier with nothing recorded.
    pub fn new() -> RecordingMedia {
        RecordingMedia::default()
    }

    /// The volumes it was told are absent.
    pub fn waiting(&self) -> Vec<String> {
        lock(&self.waiting).clone()
    }
}

impl MediaNotifier for RecordingMedia {
    fn waiting(&self, volume: &str) {
        lock(&self.waiting).push(volume.to_owned());
    }
}

/// A service gate whose state tests can inspect.
#[derive(Debug)]
pub struct FakeServiceGate {
    open: Mutex<bool>,
    closes: Mutex<u32>,
    opens: Mutex<u32>,
    fail_close: bool,
}

impl FakeServiceGate {
    /// A gate that starts open.
    pub fn open() -> FakeServiceGate {
        FakeServiceGate {
            open: Mutex::new(true),
            closes: Mutex::new(0),
            opens: Mutex::new(0),
            fail_close: false,
        }
    }

    /// A gate that starts closed.
    pub fn closed() -> FakeServiceGate {
        let gate = FakeServiceGate::open();
        *lock(&gate.open) = false;
        gate
    }

    /// Makes `close` fail.
    pub fn failing_close() -> FakeServiceGate {
        FakeServiceGate {
            fail_close: true,
            ..FakeServiceGate::open()
        }
    }

    /// How many times the gate was closed.
    pub fn closes(&self) -> u32 {
        *lock(&self.closes)
    }

    /// How many times the gate was opened.
    pub fn opens(&self) -> u32 {
        *lock(&self.opens)
    }
}

impl ServiceGate for FakeServiceGate {
    fn close(&self) -> Result<()> {
        if self.fail_close {
            return Err(BackupError::io("the fake service refuses to stop"));
        }
        *lock(&self.closes) += 1;
        *lock(&self.open) = false;
        Ok(())
    }

    fn open(&self) -> Result<()> {
        *lock(&self.opens) += 1;
        *lock(&self.open) = true;
        Ok(())
    }

    fn is_open(&self) -> bool {
        *lock(&self.open)
    }
}

/// An invariant runner with a scripted answer.
#[derive(Debug)]
pub struct FakeInvariantRunner {
    ok: bool,
    violations: Vec<String>,
    runs: Mutex<u32>,
}

impl FakeInvariantRunner {
    /// A runner that passes.
    pub fn passing() -> FakeInvariantRunner {
        FakeInvariantRunner {
            ok: true,
            violations: Vec::new(),
            runs: Mutex::new(0),
        }
    }

    /// A runner that fails with `violations`.
    pub fn failing(violations: Vec<String>) -> FakeInvariantRunner {
        FakeInvariantRunner {
            ok: false,
            violations,
            runs: Mutex::new(0),
        }
    }

    /// How many times it ran.
    pub fn runs(&self) -> u32 {
        *lock(&self.runs)
    }
}

impl InvariantRunner for FakeInvariantRunner {
    fn run<'a>(&'a self, _site: &'a dyn RestoredSite) -> BoxFuture<'a, Result<InvariantReport>> {
        Box::pin(async move {
            *lock(&self.runs) += 1;
            Ok(if self.ok {
                InvariantReport::passed("fake", 1)
            } else {
                InvariantReport::failed("fake", 1, self.violations.clone())
            })
        })
    }
}

/// A restored site that answers from stored facts and records what a restore did.
#[derive(Debug)]
pub struct FakeRestoreSite {
    tenant: String,
    runtime_schema: u64,
    data_schema: Mutex<u64>,
    pg_major: u32,
    app_set: Vec<String>,
    free_space: u64,
    empty: Mutex<bool>,
    schema_checks: Mutex<Vec<SchemaCheck>>,
    applied: Mutex<bool>,
    migrated: Mutex<Vec<(u64, u64)>>,
    reset: Mutex<bool>,
}

impl FakeRestoreSite {
    /// A site with the supplied facts.
    pub fn new(
        tenant: impl Into<String>,
        runtime_schema: u64,
        pg_major: u32,
        app_set: Vec<String>,
        free_space: u64,
        empty: bool,
    ) -> FakeRestoreSite {
        FakeRestoreSite {
            tenant: tenant.into(),
            runtime_schema,
            data_schema: Mutex::new(runtime_schema),
            pg_major,
            app_set,
            free_space,
            empty: Mutex::new(empty),
            schema_checks: Mutex::new(Vec::new()),
            applied: Mutex::new(false),
            migrated: Mutex::new(Vec::new()),
            reset: Mutex::new(false),
        }
    }

    /// Sets the schema checks the site reports.
    pub fn with_schema_checks(self, checks: Vec<SchemaCheck>) -> FakeRestoreSite {
        *lock(&self.schema_checks) = checks;
        self
    }

    /// Whether data was applied.
    pub fn applied(&self) -> bool {
        *lock(&self.applied)
    }

    /// The `(from, to)` migrations requested.
    pub fn migrated(&self) -> Vec<(u64, u64)> {
        lock(&self.migrated).clone()
    }

    /// Whether node-local state was reset.
    pub fn reset(&self) -> bool {
        *lock(&self.reset)
    }
}

impl RestoredSite for FakeRestoreSite {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn schema_version(&self) -> u64 {
        *lock(&self.data_schema)
    }

    fn app_set(&self) -> Vec<String> {
        self.app_set.clone()
    }

    fn pg_major(&self) -> u32 {
        self.pg_major
    }

    fn schema_checks(&self) -> Result<Vec<SchemaCheck>> {
        Ok(lock(&self.schema_checks).clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl crate::site::RestoreTargetSite for FakeRestoreSite {
    fn runtime_schema_version(&self) -> u64 {
        self.runtime_schema
    }

    fn free_space(&self) -> u64 {
        self.free_space
    }

    fn is_empty(&self) -> Result<bool> {
        Ok(*lock(&self.empty))
    }

    fn apply<'a>(&'a self, package: &'a PackageFile, _replace: bool) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            *lock(&self.data_schema) = package.manifest().schema_version;
            *lock(&self.empty) = false;
            *lock(&self.applied) = true;
            Ok(())
        })
    }

    fn migrate_forward<'a>(
        &'a self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'a, Result<MigrationApplied>> {
        Box::pin(async move {
            lock(&self.migrated).push((from, to));
            *lock(&self.data_schema) = to;
            Ok(MigrationApplied {
                from,
                to,
                steps: vec![format!("migrate-{from}-{to}")],
            })
        })
    }

    fn reset_node_local<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            *lock(&self.reset) = true;
            Ok(())
        })
    }
}

/// A scratch site that restores into a [`FakeRestoreSite`] and returns it.
#[derive(Debug)]
pub struct FakeScratchSite {
    site: FakeRestoreSite,
}

impl FakeScratchSite {
    /// A scratch site over `site`.
    pub fn new(site: FakeRestoreSite) -> FakeScratchSite {
        FakeScratchSite { site }
    }
}

impl crate::site::ScratchSite for FakeScratchSite {
    fn restore<'a>(
        &'a self,
        package: &'a PackageFile,
    ) -> BoxFuture<'a, Result<Box<dyn RestoredSite>>> {
        Box::pin(async move {
            *lock(&self.site.data_schema) = package.manifest().schema_version;
            let restored: Box<dyn RestoredSite> = Box::new(FakeRestoreSite::new(
                self.site.tenant.clone(),
                self.site.runtime_schema,
                self.site.pg_major,
                self.site.app_set.clone(),
                self.site.free_space,
                false,
            ));
            Ok(restored)
        })
    }
}

/// A producer that packages scripted files, for runner tests.
pub struct FakeProducer {
    work_dir: std::path::PathBuf,
    files: Vec<PackageEntry>,
    fail: Option<String>,
}

impl FakeProducer {
    /// A producer writing `files` under `work_dir`.
    pub fn new(work_dir: impl Into<std::path::PathBuf>, files: Vec<PackageEntry>) -> FakeProducer {
        FakeProducer {
            work_dir: work_dir.into(),
            files,
            fail: None,
        }
    }

    /// A producer whose `produce` fails.
    pub fn failing(
        work_dir: impl Into<std::path::PathBuf>,
        reason: impl Into<String>,
    ) -> FakeProducer {
        FakeProducer {
            work_dir: work_dir.into(),
            files: Vec::new(),
            fail: Some(reason.into()),
        }
    }
}

impl BackupProducer for FakeProducer {
    fn produce<'a>(&'a self, request: &'a BackupRequest) -> BoxFuture<'a, Result<PackagedBackup>> {
        Box::pin(async move {
            if let Some(reason) = &self.fail {
                return Err(BackupError::io(reason.clone()));
            }
            let manifest = BackupManifest {
                format: MANIFEST_FORMAT.to_owned(),
                id: request.id.clone(),
                tenant: request.job.tenant.clone(),
                created_at: BackupManifest::format_instant(request.now),
                platform_version: "0.1.0".to_owned(),
                snapshot_provider_version: "0.1.0".to_owned(),
                pg_major: 18,
                schema_version: 3,
                app_set: vec!["crm".to_owned()],
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
            crate::package::build_from_files(manifest, self.files.clone(), &self.work_dir)
        })
    }
}

/// A helper manifest for tests and for hosts that need a hand-built package.
pub fn sample_manifest(id: &str, tenant: &str, schema_version: u64) -> BackupManifest {
    BackupManifest {
        format: MANIFEST_FORMAT.to_owned(),
        id: id.to_owned(),
        tenant: tenant.to_owned(),
        created_at: "2026-10-19T12:00:00Z".to_owned(),
        platform_version: "0.1.0".to_owned(),
        snapshot_provider_version: "0.1.0".to_owned(),
        pg_major: 18,
        schema_version,
        app_set: vec!["crm".to_owned()],
        digest: "0".repeat(64),
        size: 0,
        file_count: 0,
        chain_id: "chain".to_owned(),
        chain_sequence: 1,
        parent_digest: None,
        verify: VerifyMode::Quick,
        target: "folder:/backups".to_owned(),
        encryption: None,
    }
}

/// A minimal `BackupJob` over `schedule` for tests.
pub fn sample_job(
    schedule: spot_scheduler::Schedule,
    tenant: &str,
    target: crate::model::TargetRef,
) -> BackupJob {
    BackupJob {
        tenant: tenant.to_owned(),
        schedule,
        target,
        retention: crate::retention::RetentionPolicy::default(),
        encrypt: None,
        verify: VerifyMode::Quick,
        chain_id: "chain".to_owned(),
        sequence: 1,
        parent_digest: None,
    }
}
