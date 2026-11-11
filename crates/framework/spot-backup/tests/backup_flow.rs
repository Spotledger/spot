//! End-to-end backup runs against a folder target, with verification failures and
//! retention, using the crate's public fakes.
#![allow(missing_docs)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use spot_backup::package::PackageEntry;
use spot_backup::testing::{
    FakeInvariantRunner, FakeProducer, FakeRestoreSite, FakeScratchSite, RecordingAlerts,
    RecordingHealth, RecordingMedia, sample_job,
};
use spot_backup::{
    BackupLedger, BackupRunOutcome, BackupRunner, BackupTarget, FixedSpaceProbe, FolderTarget,
    InMemoryLedger, SnapshotProducer, TargetRef, VerifyMode,
};
use spot_scheduler::{Schedule, ScheduleBuilder, TimeZone};
use std::str::FromStr;
use time::macros::datetime;

fn schedule() -> Schedule {
    let cron = spot_manifest::Cron::from_str("0 0 2 * * *").expect("cron");
    let step = spot_app_api::StepName::new("spot_backup:backup").expect("step");
    ScheduleBuilder::new("nightly")
        .cron(cron)
        .step(step)
        .zone(TimeZone::parse("UTC").expect("zone"))
        .build()
        .expect("schedule")
}

fn files() -> Vec<PackageEntry> {
    vec![
        PackageEntry {
            path: "snapshot.toml".to_owned(),
            bytes: b"tenant = \"acme\"\n".to_vec(),
        },
        PackageEntry {
            path: "data/dump".to_owned(),
            bytes: b"payload".to_vec(),
        },
    ]
}

#[tokio::test]
async fn a_quick_backup_is_written_listed_and_verified() {
    let work = tempfile::tempdir().expect("work");
    let target_dir = work.path().join("backups");
    let target = FolderTarget::new(&target_dir, Arc::new(FixedSpaceProbe(1 << 30)));
    let producer = FakeProducer::new(work.path().join("build"), files());
    let ledger = InMemoryLedger::new();
    let health = RecordingHealth::new();
    let alerts = RecordingAlerts::new();
    let media = RecordingMedia::new();
    let runner = BackupRunner {
        target: &target,
        producer: &producer,
        ledger: &ledger,
        health: &health,
        alerts: &alerts,
        notifier: &media,
        scratch: None,
        invariants: None,
    };
    let job = sample_job(
        schedule(),
        "acme",
        TargetRef::Folder {
            path: target_dir.clone(),
        },
    );
    let report = runner
        .run(&job, datetime!(2026-10-19 02:00:00 UTC))
        .await
        .expect("run");
    let BackupRunOutcome::Created {
        backup,
        verification,
    } = report.outcome
    else {
        panic!("expected a created backup");
    };
    assert!(verification.ok);
    assert_eq!(verification.mode, VerifyMode::Quick);
    assert_eq!(backup.manifest.tenant, "acme");
    assert_eq!(target.list().expect("list").len(), 1);
    assert_eq!(ledger.last_verified().expect("last").id, backup.id);
    assert!(health.conditions().is_empty());
    assert!(alerts.alerts().is_empty());
    assert!(media.waiting().is_empty());
}

#[tokio::test]
async fn a_full_verification_failure_alerts_and_keeps_the_last_good_backup() {
    let work = tempfile::tempdir().expect("work");
    let target_dir = work.path().join("backups");
    let target = FolderTarget::new(&target_dir, Arc::new(FixedSpaceProbe(1 << 30)));
    let producer = FakeProducer::new(work.path().join("build"), files());
    let ledger = InMemoryLedger::new();
    let health = RecordingHealth::new();
    let alerts = RecordingAlerts::new();
    let media = RecordingMedia::new();
    let scratch_site = FakeRestoreSite::new("acme", 3, 18, vec!["crm".to_owned()], 1 << 30, true);
    let scratch = FakeScratchSite::new(scratch_site);
    let good = FakeInvariantRunner::passing();
    let runner = BackupRunner {
        target: &target,
        producer: &producer,
        ledger: &ledger,
        health: &health,
        alerts: &alerts,
        notifier: &media,
        scratch: Some(&scratch),
        invariants: Some(&good),
    };
    let job = sample_job(
        schedule(),
        "acme",
        TargetRef::Folder {
            path: target_dir.clone(),
        },
    );
    let first = runner
        .run(&job, datetime!(2026-10-19 02:00:00 UTC))
        .await
        .expect("first");
    let BackupRunOutcome::Created {
        backup: good_backup,
        ..
    } = first.outcome
    else {
        panic!("expected a created backup");
    };
    assert_eq!(ledger.last_verified().expect("last").id, good_backup.id);

    let bad = FakeInvariantRunner::failing(vec!["seq-gapless: expected=1 actual=2".to_owned()]);
    let full_job = spot_backup::BackupJob {
        verify: VerifyMode::Full,
        ..job.clone()
    };
    let runner = BackupRunner {
        target: &target,
        producer: &producer,
        ledger: &ledger,
        health: &health,
        alerts: &alerts,
        notifier: &media,
        scratch: Some(&scratch),
        invariants: Some(&bad),
    };
    let error = runner
        .run(&full_job, datetime!(2026-10-20 02:00:00 UTC))
        .await
        .expect_err("invariants failed");
    assert_eq!(error.code(), "backup.invariants_failed");
    assert_eq!(health.conditions().len(), 1);
    assert_eq!(health.conditions()[0].code, "backup.invariants_failed");
    assert_eq!(alerts.alerts().len(), 1);
    assert_eq!(alerts.alerts()[0].code, "backup.invariants_failed");
    // The failed second backup is listed (kept for diagnosis) but is not the last good one.
    assert_eq!(target.list().expect("list").len(), 2);
    assert_eq!(ledger.last_verified().expect("last").id, good_backup.id);
    assert_eq!(
        ledger
            .records()
            .into_iter()
            .filter(|record| !record.verified)
            .count(),
        1
    );
}

#[tokio::test]
async fn retention_deletes_only_what_the_policy_allows() {
    let work = tempfile::tempdir().expect("work");
    let target_dir = work.path().join("backups");
    let target = FolderTarget::new(&target_dir, Arc::new(FixedSpaceProbe(1 << 30)));
    let producer = FakeProducer::new(work.path().join("build"), files());
    let ledger = InMemoryLedger::new();
    let health = RecordingHealth::new();
    let alerts = RecordingAlerts::new();
    let media = RecordingMedia::new();
    let runner = BackupRunner {
        target: &target,
        producer: &producer,
        ledger: &ledger,
        health: &health,
        alerts: &alerts,
        notifier: &media,
        scratch: None,
        invariants: None,
    };
    let mut job = sample_job(
        schedule(),
        "acme",
        TargetRef::Folder {
            path: target_dir.clone(),
        },
    );
    job.retention = spot_backup::RetentionPolicy {
        keep_daily: 1,
        keep_weekly: 1,
        keep_monthly: 1,
        protect_newer_than_days: 0,
    };
    let first = runner
        .run(&job, datetime!(2026-01-01 02:00:00 UTC))
        .await
        .expect("first");
    let BackupRunOutcome::Created {
        backup: first_backup,
        ..
    } = first.outcome
    else {
        panic!("created");
    };
    let second = runner
        .run(&job, datetime!(2026-10-19 02:00:00 UTC))
        .await
        .expect("second");
    assert_eq!(second.deleted, vec![first_backup.id.clone()]);
    assert_eq!(target.list().expect("list").len(), 1);
}

#[test]
fn a_snapshot_producer_can_be_constructed() {
    // The adapter compiles against the snapshot provider trait; the provider needs a
    // bundled PostgreSQL layout to take a real snapshot, so this test only pins the
    // construction seam and the schema/app facts a manifest carries.
    let work = tempfile::tempdir().expect("work");
    let clock: Arc<dyn spot_core::Clock> = Arc::new(spot_core::SystemClock::new());
    let provider = spot_snapshot::PgDumpProvider::new("/nonexistent", clock);
    let producer = SnapshotProducer::new(
        Arc::new(provider),
        spot_snapshot::SnapshotTarget::Database {
            connection: "postgres://user@localhost/db".to_owned(),
            tenant: "acme".to_owned(),
            database: "db".to_owned(),
        },
        work.path(),
        7,
        vec!["crm".to_owned()],
    );
    let job = sample_job(
        schedule(),
        "acme",
        TargetRef::Folder {
            path: "/backups".into(),
        },
    );
    let request = spot_backup::BackupRequest {
        job,
        now: datetime!(2026-10-19 02:00:00 UTC),
        id: "acme-chain-1".to_owned(),
        database: "acme".to_owned(),
    };
    // Producing would run pg_dump, so just prove the request shape is usable.
    assert_eq!(request.job.tenant, "acme");
    let _ = &producer;
}
