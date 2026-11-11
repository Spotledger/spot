//! Cuts cover all file/journal operations AND both sides of service/data hook effects.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::fixtures::{self, Fixture, NEW_FILES, OLD_FILES, layout, new_version, old_version};
use crate::journal::{Journal, Stage};
use crate::testing::{FakeDataHooks, FakeFileSystem, FakeService};
use crate::{Outcome, UpdateEngine};
use std::sync::Arc;

fn rehearsal(schema: bool, rollback: bool, cut: Option<u64>, fail: bool) -> u64 {
    let fs = Arc::new(FakeFileSystem::new());
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    // A real previous pointer and aggressive retention exercise rollback's protected set.
    let previous = semver::Version::new(0, 9, 0);
    fs.seed_version(&layout, &previous, OLD_FILES);
    fs.put_file(&layout.previous_path(), previous.to_string().as_bytes());
    fs.put_file(&layout.root().join("data/keep"), b"never move user data");
    let service = fixtures::running_service().with_faults(fs.clone());
    let hooks = FakeDataHooks::new().with_faults(fs.clone());
    let engine = UpdateEngine::new(fs.as_ref(), &service, &hooks, layout.clone());
    let mut fixture = if schema {
        Fixture::with_schema_change()
    } else {
        Fixture::good()
    };
    fixture.policy.probation_secs = 23;
    fixture.policy.retention = 1;
    if rollback {
        service.not_ready_once();
    }
    if let Some(cut) = cut {
        if fail {
            fs.fail_at(
                cut + 1,
                crate::UpdaterError::Io {
                    detail: "injected".to_owned(),
                },
            );
        } else {
            fs.cut_after(cut);
        }
    }
    let outcome = engine.apply(&fixture.request(), fixtures::NOW);
    let ops = fs.ops();
    if cut.is_none() {
        assert!(matches!(
            outcome,
            Outcome::Applied { .. } | Outcome::RolledBack { .. }
        ));
    }
    fs.reboot();
    let service = FakeService::down();
    let engine = UpdateEngine::new(fs.as_ref(), &service, &hooks, layout.clone());
    let recovered = engine.resume(fixtures::NOW + 1);
    if matches!(recovered, Outcome::NeedsAttention { .. }) {
        // An actual I/O failure while undoing an update must retain diagnostics and refuse
        // to claim success. A simulated power cut, in contrast, must always resume.
        assert!(fail, "cut {cut:?}: {recovered:?}");
        assert_eq!(
            Journal::load(fs.as_ref(), &layout.journal_path())
                .unwrap()
                .unwrap()
                .stage,
            Stage::NeedsAttention
        );
        return ops;
    }
    assert!(
        !matches!(recovered, Outcome::Refused(_) | Outcome::Interrupted { .. }),
        "cut {cut:?}: {recovered:?}"
    );
    let current = fs.pointer(&layout.current_path()).unwrap();
    assert_eq!(
        service.running(),
        Some(current.clone()),
        "cut {cut:?}: not serving"
    );
    let files = if current == new_version() {
        NEW_FILES
    } else {
        assert_eq!(current, old_version());
        OLD_FILES
    };
    for (name, bytes) in files {
        assert_eq!(
            fs.file(&layout.version_file(&current, name)),
            Some(bytes.to_vec()),
            "cut {cut:?}: incomplete current"
        );
    }
    assert_eq!(
        hooks.epoch(),
        if schema && current == new_version() {
            3
        } else {
            2
        },
        "cut {cut:?}: code/data mismatch"
    );
    assert_eq!(
        fs.file(&layout.root().join("data/keep")),
        Some(b"never move user data".to_vec())
    );
    assert!(fs.has_dir(&layout.version_dir(&old_version())));
    assert!(fs.has_dir(&layout.version_dir(&previous)));
    if let Some(journal) = Journal::load(fs.as_ref(), &layout.journal_path()).unwrap() {
        assert_eq!(journal.stage, Stage::Committed);
        assert_eq!(current, new_version());
        assert_eq!(journal.recovery_policy.probation_secs, 23);
    }
    let before = (
        fs.pointer(&layout.current_path()),
        fs.pointer(&layout.previous_path()),
        hooks.epoch(),
    );
    let _ = engine.resume(fixtures::NOW + 2);
    assert_eq!(
        before,
        (
            fs.pointer(&layout.current_path()),
            fs.pointer(&layout.previous_path()),
            hooks.epoch()
        )
    );
    ops
}

#[test]
fn cuts_at_every_forward_and_rollback_boundary_with_and_without_migration() {
    for schema in [false, true] {
        for rollback in [false, true] {
            let total = rehearsal(schema, rollback, None, false);
            assert!(
                total > 30,
                "rehearsal must exercise the real engine, got {total}"
            );
            eprintln!("exhaustive schema={schema} rollback={rollback}: {total} boundaries");
            for cut in 0..=total {
                rehearsal(schema, rollback, Some(cut), false);
            }
        }
    }
}

#[test]
fn errors_at_every_forward_and_rollback_boundary_preserve_safety() {
    for schema in [false, true] {
        for rollback in [false, true] {
            let total = rehearsal(schema, rollback, None, false);
            for cut in 0..total {
                rehearsal(schema, rollback, Some(cut), true);
            }
        }
    }
}
