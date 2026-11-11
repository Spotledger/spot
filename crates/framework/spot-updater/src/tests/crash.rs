//! Crash-resume: a power cut at every operation, the post-commit probation window, and
//! idempotency.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::engine::{Outcome, UpdateEngine};
use crate::journal::{Journal, Stage, Step};
use crate::testing::{FakeFileSystem, FakeService};

use super::fixtures::{self, Fixture, NEW_FILES, OLD_FILES, layout, new_version, old_version};

/// Runs a clean update once to learn how many filesystem operations it takes.
fn operation_count() -> u64 {
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
    fs.ops()
}

/// A power cut after `n` operations, a reboot, then `resume`, always leaves exactly one of the
/// old version (intact, running) or the new version (committed, running), never a mix.
#[test]
fn a_power_cut_at_every_operation_resumes_consistently() {
    let total = operation_count();
    assert!(total > 5, "the run should take a handful of operations");

    for n in 0..=total {
        let fs = FakeFileSystem::new();
        let layout = layout();
        fixtures::seed_installed(&fs, &layout);
        let service = fixtures::running_service();
        let hooks = fixtures::hooks();
        let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
        let fixture = Fixture::good();

        fs.cut_after(n);
        let _ = engine.apply(&fixture.request(), fixtures::NOW);

        // The machine reboots: the disk survives, the service comes back down.
        fs.reboot();
        let service = FakeService::down();
        let hooks = fixtures::hooks();
        let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
        let outcome = engine.resume(fixtures::NOW + 1);
        assert!(
            matches!(
                outcome,
                Outcome::Applied { .. }
                    | Outcome::Kept { .. }
                    | Outcome::RolledBack { .. }
                    | Outcome::NothingToDo
            ),
            "cut {n}: unexpected resume outcome {outcome:?}"
        );

        let current = fs
            .pointer(&layout.current_path())
            .unwrap_or_else(|| panic!("cut {n}: no current pointer"));
        assert!(
            current == old_version() || current == new_version(),
            "cut {n}: current names {current}"
        );
        assert!(
            fs.has_dir(&layout.version_dir(&current)),
            "cut {n}: current names a missing version"
        );
        let files = if current == new_version() {
            NEW_FILES
        } else {
            OLD_FILES
        };
        for (path, bytes) in files {
            assert_eq!(
                fs.file(&layout.version_file(&current, path)),
                Some(bytes.to_vec()),
                "cut {n}: {path} is not intact for {current}"
            );
        }
        assert!(
            !fs.has_dir(&layout.staging_path(&new_version())),
            "cut {n}: staging survived"
        );
        assert_eq!(
            service.running(),
            Some(current),
            "cut {n}: current must be serving"
        );

        // Resume is idempotent: a second call changes nothing.
        let before = (
            fs.pointer(&layout.current_path()),
            fs.pointer(&layout.previous_path()),
            service.running(),
        );
        let _ = engine.resume(fixtures::NOW + 2);
        let after = (
            fs.pointer(&layout.current_path()),
            fs.pointer(&layout.previous_path()),
            service.running(),
        );
        assert_eq!(before, after, "cut {n}: resume was not idempotent");
    }
}

/// A failure injected at every operation leaves the old version serving and the pointers
/// unchanged, or is refused before anything changes.
#[test]
fn a_failure_at_every_operation_leaves_the_install_untouched() {
    let total = operation_count();
    for n in 1..=total {
        let fs = FakeFileSystem::new();
        let layout = layout();
        fixtures::seed_installed(&fs, &layout);
        let service = fixtures::running_service();
        let hooks = fixtures::hooks();
        let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
        let fixture = Fixture::good();

        fs.fail_at(
            n,
            crate::error::UpdaterError::Io {
                detail: "injected".to_owned(),
            },
        );
        let outcome = engine.apply(&fixture.request(), fixtures::NOW);
        match outcome {
            Outcome::Refused(_) | Outcome::RolledBack { .. } => {
                assert_eq!(
                    fs.pointer(&layout.current_path()),
                    Some(old_version()),
                    "op {n}: current moved"
                );
                assert_eq!(
                    fs.pointer(&layout.previous_path()),
                    None,
                    "op {n}: previous changed"
                );
                assert_eq!(
                    service.running(),
                    Some(old_version()),
                    "op {n}: the old version is not serving"
                );
                assert!(
                    !fs.has_dir(&layout.staging_path(&new_version())),
                    "op {n}: staging survived"
                );
            }
            // After the commit point the update is applied; the injection hit best-effort
            // pruning, which cannot roll the update back.
            Outcome::Applied { .. } => {
                assert_eq!(fs.pointer(&layout.current_path()), Some(new_version()));
                assert_eq!(fs.pointer(&layout.previous_path()), Some(old_version()));
            }
            other => panic!("op {n}: unexpected outcome {other:?}"),
        }
    }
}

/// A crash of the new version inside the probation window rolls back on the next tick.
#[test]
fn a_post_commit_crash_within_probation_rolls_back() {
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
    service.crash();

    match engine.tick(fixtures::NOW + 10) {
        Outcome::RolledBack { failed_step, .. } => assert_eq!(failed_step, Step::Health),
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
    assert_eq!(fs.pointer(&layout.previous_path()), None);
    assert_eq!(service.running(), Some(old_version()));
    assert!(!fs.has_dir(&layout.version_dir(&new_version())));
}

/// A crash of the new version after the probation window is kept, not rolled back.
#[test]
fn a_post_commit_crash_after_probation_is_kept() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();
    let probation = fixture.policy.probation_secs;

    assert!(matches!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));
    service.crash();

    let past = fixtures::NOW + i64::try_from(probation).unwrap() + 1;
    assert!(matches!(engine.tick(past), Outcome::Kept { .. }));
    assert_eq!(fs.pointer(&layout.current_path()), Some(new_version()));
    assert_eq!(fs.pointer(&layout.previous_path()), Some(old_version()));
    assert!(
        Journal::load(&fs, &layout.journal_path())
            .unwrap()
            .is_none()
    );
}

/// A resume with no journal does nothing; the same is true of a second resume.
#[test]
fn resume_without_a_journal_is_a_no_op() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());

    assert_eq!(engine.resume(fixtures::NOW), Outcome::NothingToDo);
    assert_eq!(engine.resume(fixtures::NOW + 1), Outcome::NothingToDo);
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
}

/// A journal left mid-migrate is rolled back and restored on resume.
#[test]
fn a_resume_mid_migrate_restores() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::with_schema_change();

    // Simulate a crash right after the migrate hook started: the journal says so.
    let mut journal = Journal::new(new_version(), Some(old_version()), None);
    journal.step = Step::Migrate;
    journal.quiesced = true;
    journal.migrate_started = true;
    journal.store(&fs, &layout.journal_path()).unwrap();

    match engine.resume(fixtures::NOW) {
        Outcome::RolledBack { failed_step, .. } => assert_eq!(failed_step, Step::Migrate),
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(hooks.restores(), 1);
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
    assert_eq!(service.running(), Some(old_version()));
    let _ = fixture;
}

/// After a resume finishes a rollback, the journal is gone (Stage::RolledBack is cleared).
#[test]
fn a_rolled_back_journal_is_cleared_on_resume() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());

    let mut journal = Journal::new(new_version(), Some(old_version()), None);
    journal.stage = Stage::RolledBack;
    journal.store(&fs, &layout.journal_path()).unwrap();

    assert_eq!(engine.resume(fixtures::NOW), Outcome::NothingToDo);
    assert!(
        Journal::load(&fs, &layout.journal_path())
            .unwrap()
            .is_none()
    );
}
