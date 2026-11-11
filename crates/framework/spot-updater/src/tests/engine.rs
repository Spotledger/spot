//! The forward update path: apply, backups and migrations, retention, and rollback.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::engine::{Outcome, UpdateEngine, requires_schema_change};
use crate::journal::Journal;
use crate::layout::Layout;
use crate::testing::{FakeDataHooks, FakeFileSystem};

use super::fixtures::{self, Fixture, NEW_FILES, OLD_FILES, layout, new_version, old_version};

/// A good update applies: the new version serves, `current` moves, `previous` remembers.
#[test]
fn a_good_update_applies() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    let outcome = engine.apply(&fixture.request(), fixtures::NOW);

    assert_eq!(
        outcome,
        Outcome::Applied {
            version: new_version()
        }
    );
    assert_eq!(fs.pointer(&layout.current_path()), Some(new_version()));
    assert_eq!(fs.pointer(&layout.previous_path()), Some(old_version()));
    assert_eq!(
        fs.file(&layout.version_file(&new_version(), "bin/spot")),
        Some(b"new-binary".to_vec())
    );
    assert_eq!(service.running(), Some(new_version()));
    // The commit is recorded and still open for probation.
    let journal = Journal::load(&fs, &layout.journal_path()).unwrap().unwrap();
    assert_eq!(journal.stage, crate::journal::Stage::Committed);
    assert_eq!(journal.committed_at, Some(fixtures::NOW));
}

/// Flipped bytes are refused before anything outside staging changes (here: before anything at
/// all), and the old version keeps serving.
#[test]
fn a_flipped_payload_byte_is_refused_before_any_disk_change() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();
    let tampered = fixture.tampered_payload();
    let mut request = fixture.request();
    request.payload = &tampered;

    let outcome = engine.apply(&request, fixtures::NOW);

    assert_eq!(
        outcome,
        Outcome::Refused(crate::error::UpdaterErrorCode::PayloadDigestMismatch)
    );
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
    assert_eq!(fs.pointer(&layout.previous_path()), None);
    assert!(!fs.has_dir(&layout.version_dir(&new_version())));
    assert!(!fs.has_dir(&layout.staging_path(&new_version())));
    assert_eq!(service.running(), Some(old_version()));
    assert!(
        Journal::load(&fs, &layout.journal_path())
            .unwrap()
            .is_none()
    );
}

/// Applying the same update a second time changes nothing.
#[test]
fn the_same_update_twice_is_a_no_op() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    assert!(matches!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));
    let started_after_first = service.started();

    // The committed update is still open for probation: a second apply is a no-op.
    assert_eq!(
        engine.apply(&fixture.request(), fixtures::NOW + 1),
        Outcome::NothingToDo
    );
    assert_eq!(service.started(), started_after_first);
    assert_eq!(fs.pointer(&layout.current_path()), Some(new_version()));

    // Once probation closes and the journal is gone, the decision skips the same version.
    assert!(matches!(
        engine.tick(fixtures::NOW + 10_000),
        Outcome::Kept { .. }
    ));
    let mut second = fixture.request();
    second.installed.version = new_version();
    assert_eq!(
        engine.apply(&second, fixtures::NOW + 10_001),
        Outcome::Skipped("this version is already installed".to_owned())
    );
}

/// A health check that fails rolls back to the old version.
#[test]
fn a_failed_health_check_rolls_back() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    service.unhealthy();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    let outcome = engine.apply(&fixture.request(), fixtures::NOW);

    match outcome {
        Outcome::RolledBack { failed_step, .. } => {
            assert_eq!(failed_step, crate::journal::Step::Health);
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
    assert_eq!(fs.pointer(&layout.previous_path()), None);
    assert!(!fs.has_dir(&layout.version_dir(&new_version())));
    assert!(!fs.has_dir(&layout.staging_path(&new_version())));
    assert_eq!(service.running(), Some(old_version()));
}

/// A start that fails rolls back to the old version.
#[test]
fn a_start_that_fails_rolls_back() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    service.fail_next_start();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    match engine.apply(&fixture.request(), fixtures::NOW) {
        Outcome::RolledBack { failed_step, .. } => {
            assert_eq!(failed_step, crate::journal::Step::StartNew);
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
    assert_eq!(service.running(), Some(old_version()));
}

/// A service that never becomes ready rolls back.
#[test]
fn a_start_that_never_becomes_ready_rolls_back() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    service.not_ready_once();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    match engine.apply(&fixture.request(), fixtures::NOW) {
        Outcome::RolledBack { failed_step, .. } => {
            assert_eq!(failed_step, crate::journal::Step::Health);
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
    assert_eq!(service.running(), Some(old_version()));
}

/// A schema change runs the backup then the migration; nothing is restored on success.
#[test]
fn a_schema_change_runs_the_backup_then_the_migration() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::with_schema_change();
    assert!(requires_schema_change(
        &fixture.installed,
        &fixture.manifest
    ));

    assert!(matches!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));
    assert_eq!(hooks.backups(), 1);
    assert_eq!(hooks.migrations(), 1);
    assert_eq!(hooks.restores(), 0);
}

/// An update that changes no schema never backs up.
#[test]
fn no_schema_change_skips_the_backup() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    assert!(matches!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));
    assert_eq!(hooks.backups(), 0);
    assert_eq!(hooks.migrations(), 0);
}

/// A migration that fails rolls back and restores the data from the backup.
#[test]
fn a_failed_migration_rolls_back_and_restores() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    hooks.fail_migrate();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::with_schema_change();

    match engine.apply(&fixture.request(), fixtures::NOW) {
        Outcome::RolledBack { failed_step, .. } => {
            assert_eq!(failed_step, crate::journal::Step::Migrate);
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(hooks.backups(), 1);
    assert_eq!(hooks.restores(), 1);
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
    assert_eq!(service.running(), Some(old_version()));
}

/// A backup that fails rolls back before touching the schema.
#[test]
fn a_failed_backup_rolls_back() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    hooks.fail_backup();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::with_schema_change();

    match engine.apply(&fixture.request(), fixtures::NOW) {
        Outcome::RolledBack { failed_step, .. } => {
            assert_eq!(failed_step, crate::journal::Step::Backup);
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(hooks.migrations(), 0);
    assert_eq!(hooks.restores(), 0);
    assert_eq!(service.running(), Some(old_version()));
}

/// Retention keeps the newest versions and never deletes `current` or `previous`.
#[test]
fn retention_keeps_the_last_versions() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    // Older versions that retention should remove.
    for minor in 1..=4 {
        let version = semver::Version::parse(&format!("0.{minor}.0")).unwrap();
        fs.seed_version(&layout, &version, OLD_FILES);
    }
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let mut fixture = Fixture::good();
    fixture.policy.retention = 2;

    assert!(matches!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));

    assert!(fs.has_dir(&layout.version_dir(&old_version())));
    assert!(fs.has_dir(&layout.version_dir(&new_version())));
    for minor in 1..=4 {
        let version = semver::Version::parse(&format!("0.{minor}.0")).unwrap();
        assert!(
            !fs.has_dir(&layout.version_dir(&version)),
            "{version} should have been pruned"
        );
    }
}

/// A rollback never points `current` at a version that is not on disk; if the previous version
/// is missing it needs attention and leaves `current` on a complete version.
#[test]
fn a_rollback_with_a_missing_previous_version_needs_attention() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    // `current` names 1.0.0 but its directory is gone (a pre-existing corruption).
    fs.seed_current(&layout, &old_version());
    let service = fixtures::running_service();
    service.unhealthy();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    match engine.apply(&fixture.request(), fixtures::NOW) {
        Outcome::NeedsAttention { .. } => {}
        other => panic!("expected needs-attention, got {other:?}"),
    }
    // `current` still names a complete version (the new one), never the missing 1.0.0.
    assert_eq!(fs.pointer(&layout.current_path()), Some(new_version()));
    assert!(fs.has_dir(&layout.version_dir(&new_version())));
    let journal = Journal::load(&fs, &layout.journal_path()).unwrap().unwrap();
    assert_eq!(journal.stage, crate::journal::Stage::NeedsAttention);
}

/// A rollback that cannot restart the old version needs attention.
#[test]
fn a_failed_rollback_needs_attention() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    service.fail_start();
    service.fail_stop();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    match engine.apply(&fixture.request(), fixtures::NOW) {
        Outcome::NeedsAttention { .. } => {}
        other => panic!("expected needs-attention, got {other:?}"),
    }
    let journal = Journal::load(&fs, &layout.journal_path()).unwrap().unwrap();
    assert_eq!(journal.stage, crate::journal::Stage::NeedsAttention);
    assert!(journal.note.is_some());
}

/// The fake filesystem's power-cut injection is exercised by the crash tests, but a plain run
/// must never see it.
#[test]
fn a_clean_run_writes_the_journal_durably() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = FakeDataHooks::new();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    assert!(matches!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));
    assert!(fs.durable_writes() >= 2);
    assert_eq!(NEW_FILES.len(), 2);
    let _: &Layout = &layout;
}
