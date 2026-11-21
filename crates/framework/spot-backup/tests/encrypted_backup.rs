//! Encrypted backups end to end: the runner, a folder target, a durable file ledger and
//! a key provider, using the crate's public API and fakes (WP-PRD-04.T6).
#![allow(missing_docs)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Read;
use std::str::FromStr;
use std::sync::Arc;

use spot_backup::package::PackageEntry;
use spot_backup::testing::{
    FakeInvariantRunner, FakeProducer, FakeRestoreSite, FakeScratchSite, RecordingAlerts,
    RecordingHealth, RecordingMedia, sample_job,
};
use spot_backup::{
    BackupJob, BackupLedger, BackupRunOutcome, BackupRunner, BackupTarget, FileLedger,
    FixedSpaceProbe, FolderTarget, KeyRef, TargetRef, VerifyMode, encrypting,
};
use spot_scheduler::{Schedule, ScheduleBuilder, TimeZone};
use spot_secrets::{KeyProvider, MemoryKeyring};
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
            bytes: b"PLAINTEXT-MARKER-0123456789".to_vec(),
        },
    ]
}

fn scratch() -> (FakeScratchSite, FakeInvariantRunner) {
    let site = FakeRestoreSite::new("acme", 3, 18, vec!["crm".to_owned()], 1 << 30, true);
    (FakeScratchSite::new(site), FakeInvariantRunner::passing())
}

fn job(verify: VerifyMode, target_dir: &std::path::Path, key: Option<KeyRef>) -> BackupJob {
    let mut job = sample_job(
        schedule(),
        "acme",
        TargetRef::Folder {
            path: target_dir.to_path_buf(),
        },
    );
    job.verify = verify;
    job.encrypt = key;
    job
}

fn key_of(provider: &MemoryKeyring) -> KeyRef {
    let key = provider.current();
    KeyRef {
        id: key.id,
        version: key.version,
    }
}

#[tokio::test]
async fn an_encrypted_backup_is_stored_encrypted_and_verifies_quick_and_full() {
    for verify in [VerifyMode::Quick, VerifyMode::Full] {
        let work = tempfile::tempdir().expect("work");
        let target_dir = work.path().join("backups");
        let target = FolderTarget::new(&target_dir, Arc::new(FixedSpaceProbe(1 << 30)));
        let keyring = Arc::new(MemoryKeyring::new("backup-kek").expect("keyring"));
        let producer = encrypting(
            Arc::new(FakeProducer::new(work.path().join("build"), files())),
            keyring.clone(),
        );
        let ledger = FileLedger::open(&target_dir).expect("ledger");
        let _lock = FileLedger::lock(&target_dir).expect("lock");
        let health = RecordingHealth::new();
        let alerts = RecordingAlerts::new();
        let media = RecordingMedia::new();
        let (scratch_site, invariants) = scratch();
        let runner = BackupRunner {
            target: &target,
            producer: &producer,
            ledger: &ledger,
            health: &health,
            alerts: &alerts,
            notifier: &media,
            scratch: Some(&scratch_site),
            invariants: Some(&invariants),
        };
        let key = key_of(&keyring);
        let report = runner
            .run(
                &job(verify, &target_dir, Some(key.clone())),
                datetime!(2026-10-19 02:00:00 UTC),
            )
            .await
            .expect("run");
        let BackupRunOutcome::Created {
            backup,
            verification,
        } = report.outcome
        else {
            panic!("expected a created backup");
        };
        assert!(verification.ok, "{:?}", verification.failure);
        assert_eq!(verification.mode, verify);
        assert_eq!(backup.manifest.encryption, Some(key));

        // The stored bytes hold no plaintext.
        let mut stored = Vec::new();
        target
            .get(&backup.id)
            .expect("get")
            .read_to_end(&mut stored)
            .expect("read");
        assert!(
            !stored
                .windows(b"PLAINTEXT-MARKER".len())
                .any(|window| window == b"PLAINTEXT-MARKER"),
            "the stored package must not contain the payload"
        );

        // The verified record survives a reload of the ledger.
        let reloaded = FileLedger::open(&target_dir).expect("reload");
        assert_eq!(reloaded.last_verified().expect("last").id, backup.id);
        assert!(health.conditions().is_empty());
        assert!(alerts.alerts().is_empty());
        if verify == VerifyMode::Full {
            assert_eq!(invariants.runs(), 1);
        }

        // No decrypted scratch package is left in the build directory.
        let leftovers: Vec<_> = std::fs::read_dir(work.path().join("build"))
            .expect("build dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(".opened-") || name.starts_with(".sealed-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[tokio::test]
async fn a_job_that_asks_for_encryption_without_a_provider_writes_nothing() {
    let work = tempfile::tempdir().expect("work");
    let target_dir = work.path().join("backups");
    let target = FolderTarget::new(&target_dir, Arc::new(FixedSpaceProbe(1 << 30)));
    let producer = FakeProducer::new(work.path().join("build"), files());
    let ledger = FileLedger::open(&target_dir).expect("ledger");
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
    let key = KeyRef {
        id: "backup-kek".to_owned(),
        version: 1,
    };
    let error = runner
        .run(
            &job(VerifyMode::Quick, &target_dir, Some(key)),
            datetime!(2026-10-19 02:00:00 UTC),
        )
        .await
        .expect_err("no provider");
    assert_eq!(error.code(), "backup.encryption_unavailable");
    assert!(target.list().expect("list").is_empty());
    assert!(ledger.records().is_empty());
    let plain_left: Vec<_> = std::fs::read_dir(work.path().join("build"))
        .expect("build dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(
        plain_left.is_empty(),
        "the plaintext package is removed: {plain_left:?}"
    );
}

#[tokio::test]
async fn full_verification_fails_when_the_key_is_retired() {
    let work = tempfile::tempdir().expect("work");
    let target_dir = work.path().join("backups");
    let target = FolderTarget::new(&target_dir, Arc::new(FixedSpaceProbe(1 << 30)));
    let keyring = Arc::new(MemoryKeyring::new("backup-kek").expect("keyring"));
    let key = key_of(&keyring);
    let producer = encrypting(
        Arc::new(FakeProducer::new(work.path().join("build"), files())),
        keyring.clone(),
    );
    // Seal first, then rotate and retire the key before anything verifies the package.
    let first = {
        let ledger = FileLedger::open(&target_dir).expect("ledger");
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
        runner
            .run(
                &job(VerifyMode::Quick, &target_dir, Some(key)),
                datetime!(2026-10-19 02:00:00 UTC),
            )
            .await
            .expect("quick run")
    };
    let BackupRunOutcome::Created { backup, .. } = first.outcome else {
        panic!("expected a created backup");
    };
    keyring.rotate().expect("rotate");
    keyring.retire(1, false).expect("retire");

    let package = spot_backup::package::open(&target_dir.join(format!("{}.spotbak", backup.id)))
        .expect("open package");
    let (scratch_site, invariants) = scratch();
    let report = spot_backup::verify::full_with_provider(
        &package,
        keyring.as_ref(),
        &scratch_site,
        &invariants,
    )
    .await
    .expect("report");
    assert!(!report.ok);
    let failure = report.failure.expect("failure");
    assert!(
        failure.contains("backup.encryption_unavailable"),
        "{failure}"
    );
    assert!(failure.contains("retired"), "{failure}");
    assert_eq!(invariants.runs(), 0);

    // Plain `full` has no provider and says so instead of guessing.
    let report = spot_backup::verify::full(&package, &scratch_site, &invariants)
        .await
        .expect("report");
    assert!(!report.ok);
}
