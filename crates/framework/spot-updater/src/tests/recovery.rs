//! Regressions found during the inherited implementation audit.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::fixtures::{self, Fixture, OLD_FILES, layout, new_version, old_version};
use crate::journal::{Journal, Stage};
use crate::testing::FakeService;
use crate::{DataHooks, FileSystem, Outcome, UpdateEngine, UpdaterError};

#[test]
fn recovery_uses_the_committed_policy() {
    let fs = crate::testing::FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let mut fixture = Fixture::good();
    fixture.policy.probation_secs = 1;
    assert!(matches!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));
    service.crash();
    assert!(matches!(
        engine.tick(fixtures::NOW + 2),
        Outcome::Kept { .. }
    ));
    assert_eq!(fs.pointer(&layout.current_path()), Some(new_version()));
}

#[test]
fn probation_preserves_the_preexisting_previous_version() {
    let fs = crate::testing::FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let previous = semver::Version::new(0, 9, 0);
    fs.seed_version(&layout, &previous, OLD_FILES);
    fs.put_file(&layout.previous_path(), previous.to_string().as_bytes());
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let mut fixture = Fixture::good();
    fixture.policy.retention = 1;
    assert!(matches!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));
    assert!(fs.has_dir(&layout.version_dir(&previous)));
    service.crash();
    assert!(matches!(
        engine.tick(fixtures::NOW + 1),
        Outcome::RolledBack { .. }
    ));
    assert_eq!(fs.pointer(&layout.previous_path()), Some(previous.clone()));
    assert!(fs.has_dir(&layout.version_dir(&previous)));
}

struct SafeRestore<'a>(&'a FakeService);
impl DataHooks for SafeRestore<'_> {
    fn backup(&self) -> Result<(), UpdaterError> {
        Ok(())
    }
    fn migrate(&self, _: u64, _: u64) -> Result<(), UpdaterError> {
        Ok(())
    }
    fn restore(&self) -> Result<(), UpdaterError> {
        assert!(self.0.running().is_none(), "restoring live data is unsafe");
        Ok(())
    }
}

#[test]
fn rollback_stops_the_new_process_before_restoring_data() {
    let fs = crate::testing::FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = SafeRestore(&service);
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout);
    service.not_ready_once();
    assert!(matches!(
        engine.apply(&Fixture::with_schema_change().request(), fixtures::NOW),
        Outcome::RolledBack { .. }
    ));
}

#[test]
fn stale_installed_metadata_cannot_authorize_a_downgrade() {
    let fs = crate::testing::FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let actual = semver::Version::new(3, 0, 0);
    fs.seed_version(&layout, &actual, OLD_FILES);
    fs.seed_current(&layout, &actual);
    let service = FakeService::new(Some(actual.clone()));
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    assert!(matches!(
        engine.apply(&Fixture::good().request(), fixtures::NOW),
        Outcome::Refused(_)
    ));
    assert_eq!(fs.pointer(&layout.current_path()), Some(actual));
}

#[test]
fn resume_before_quiesce_starts_the_old_version_after_reboot() {
    let fs = crate::testing::FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    Journal::new(new_version(), Some(old_version()), None)
        .store(&fs, &layout.journal_path())
        .unwrap();
    let service = FakeService::down();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout);
    assert!(matches!(
        engine.resume(fixtures::NOW),
        Outcome::RolledBack { .. }
    ));
    assert_eq!(service.running(), Some(old_version()));
}

#[test]
fn a_power_cut_during_probation_rollback_cannot_resurrect_the_new_commit() {
    // Every file operation on the rollback path, including its final journal writes.
    for n in 0..40 {
        let fs = crate::testing::FakeFileSystem::new();
        let layout = layout();
        fixtures::seed_installed(&fs, &layout);
        let service = fixtures::running_service();
        let hooks = fixtures::hooks();
        let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
        assert!(matches!(
            engine.apply(&Fixture::with_schema_change().request(), fixtures::NOW),
            Outcome::Applied { .. }
        ));
        service.crash();
        fs.reboot();
        fs.cut_after(n);
        let _ = engine.tick(fixtures::NOW + 1);
        fs.reboot();
        let service = FakeService::down();
        let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
        let result = engine.resume(fixtures::NOW + 2);
        assert!(
            !matches!(result, Outcome::NeedsAttention { .. } | Outcome::Refused(_)),
            "cut {n}: {result:?}"
        );
        let current = fs.pointer(&layout.current_path()).unwrap();
        assert!(
            fs.has_dir(&layout.version_dir(&current)),
            "cut {n}: missing current"
        );
        assert_eq!(service.running(), Some(current), "cut {n}: not serving");
        if let Some(journal) = Journal::load(&fs, &layout.journal_path()).unwrap() {
            assert_eq!(journal.stage, Stage::Committed);
        }
    }
}

#[test]
fn failed_restore_never_starts_old_code_on_new_data() {
    let fs = crate::testing::FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    hooks.fail_restore();
    service.not_ready_once();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    assert!(matches!(
        engine.apply(&Fixture::with_schema_change().request(), fixtures::NOW),
        Outcome::NeedsAttention { .. }
    ));
    assert_eq!(service.running(), None);
    assert_eq!(
        Journal::load(&fs, &layout.journal_path())
            .unwrap()
            .unwrap()
            .stage,
        Stage::NeedsAttention
    );
    assert!(fs.exists(&layout.version_dir(&old_version())).unwrap());
}

#[test]
fn tick_observes_a_crash_before_a_restart_can_hide_it() {
    let fs = crate::testing::FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    assert!(matches!(
        engine.apply(&Fixture::good().request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));
    // Unlike crash(), stop does not latch unhealthy: starting would conceal the outage.
    crate::ServiceControl::stop(&service).unwrap();
    assert!(matches!(
        engine.tick(fixtures::NOW + 1),
        Outcome::RolledBack { .. }
    ));
    assert_eq!(service.running(), Some(old_version()));
}
