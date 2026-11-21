//! The coordination sequence and its failure paths, on the in-memory fakes: a failure at
//! every step leaves PostgreSQL out of backup mode and no orphan shadow copy behind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use spot_agent_windows_sys::{
    BackupHorizon, BackupStep, FakePgBackup, FakeVss, RetentionPolicy, SnapshotCoordinator,
    VolumeId, VolumeSet, VssApi, VssStep, WriterSet, apply_retention,
};

/// A fake VSS with a shadow root that mirrors a `data` directory, a fake backup port, and
/// the temporary directory that owns both.
struct Fixture {
    vss: FakeVss,
    backup: FakePgBackup,
    source: PathBuf,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("data");
    std::fs::create_dir_all(&source).unwrap();
    let shadow_root = dir.path().join("shadow");
    std::fs::create_dir_all(shadow_root.join("data")).unwrap();
    let vss = FakeVss::new(VolumeId::new("C:\\").unwrap(), &shadow_root);
    let backup = FakePgBackup::new(BackupHorizon {
        xmin: 100,
        epoch: 7,
    });
    Fixture {
        vss,
        backup,
        source,
        _dir: dir,
    }
}

fn volumes() -> VolumeSet {
    VolumeSet::one(VolumeId::new("C:\\").unwrap())
}

fn run(fixture: &Fixture) -> Result<(), spot_agent_windows_sys::WindowsSysError> {
    let coordinator = SnapshotCoordinator::new(&fixture.vss, &fixture.backup);
    let coordinated = coordinator.run(&volumes(), &WriterSet::all(), &fixture.source, 1_000)?;
    // The caller reads through the exposed path and then finishes the shadow.
    assert!(
        coordinated
            .exposed_path()
            .starts_with(fixture._dir.path().join("shadow"))
    );
    coordinated.finish()
}

#[test]
fn a_successful_run_leaves_no_backup_mode_and_no_shadow() {
    let fixture = fixture();
    run(&fixture).unwrap();
    assert!(!fixture.backup.is_active(), "backup mode must be off");
    assert_eq!(fixture.backup.starts(), 1);
    assert_eq!(fixture.backup.stops(), 1);
    assert_eq!(fixture.vss.count(), 0, "the shadow must be deleted");
    assert_eq!(fixture.vss.expose_calls(), 1);
    assert_eq!(fixture.vss.unexpose_calls(), 1);
}

#[test]
fn a_failure_at_create_leaves_no_backup_mode_and_no_shadow() {
    let fixture = fixture();
    fixture.vss.fail_at(VssStep::Create);
    assert!(run(&fixture).is_err());
    assert!(!fixture.backup.is_active(), "backup mode must be off");
    assert_eq!(fixture.backup.stops(), 1, "the guard stops backup mode");
    assert_eq!(fixture.vss.count(), 0);
}

#[test]
fn a_failure_at_expose_deletes_the_shadow_and_stops_backup_mode() {
    let fixture = fixture();
    fixture.vss.fail_at(VssStep::Expose);
    assert!(run(&fixture).is_err());
    assert!(!fixture.backup.is_active(), "backup mode must be off");
    assert_eq!(fixture.backup.stops(), 1);
    assert_eq!(fixture.vss.count(), 0, "no orphan shadow copy");
}

#[test]
fn a_failure_at_backup_start_creates_nothing() {
    let fixture = fixture();
    fixture.backup.fail_at(BackupStep::Start);
    assert!(run(&fixture).is_err());
    assert_eq!(
        fixture.backup.starts(),
        0,
        "the start failed before it counted"
    );
    assert_eq!(fixture.backup.stops(), 0, "nothing to stop");
    assert_eq!(fixture.vss.count(), 0);
}

#[test]
fn a_failure_at_backup_stop_is_retried_and_leaves_no_backup_mode() {
    let fixture = fixture();
    fixture.backup.fail_at(BackupStep::Stop);
    assert!(run(&fixture).is_err());
    assert!(
        !fixture.backup.is_active(),
        "the guard retries the stop on drop"
    );
    assert_eq!(fixture.backup.stops(), 2, "one explicit stop, one retry");
    assert_eq!(fixture.vss.count(), 0, "no orphan shadow copy");
}

#[test]
fn an_empty_volume_set_is_refused() {
    let volumes: Result<VolumeSet, _> = VolumeSet::new(Vec::new());
    let error = volumes.expect_err("an empty set is refused");
    assert_eq!(error.code(), "agent_windows_sys.volume_set_empty");
}

#[test]
fn retention_deletes_the_oldest_first() {
    let fixture = fixture();
    let volumes = volumes();
    let oldest = fixture
        .vss
        .create(&volumes, &WriterSet::all(), 100)
        .unwrap();
    let middle = fixture
        .vss
        .create(&volumes, &WriterSet::all(), 200)
        .unwrap();
    let newest = fixture
        .vss
        .create(&volumes, &WriterSet::all(), 300)
        .unwrap();

    let deleted = apply_retention(&fixture.vss, &RetentionPolicy::keep(1)).unwrap();
    assert_eq!(deleted, vec![oldest.id, middle.id]);
    assert_eq!(fixture.vss.ids(), vec![newest.id]);
}

#[test]
fn retention_keeps_everything_when_within_the_policy() {
    let fixture = fixture();
    let volumes = volumes();
    fixture
        .vss
        .create(&volumes, &WriterSet::all(), 100)
        .unwrap();
    let deleted = apply_retention(&fixture.vss, &RetentionPolicy::keep(5)).unwrap();
    assert!(deleted.is_empty());
    assert_eq!(fixture.vss.count(), 1);
}
