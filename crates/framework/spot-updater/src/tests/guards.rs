//! The guards: free space before unpacking, disk-full during unpacking, and the archive's path
//! and bomb safety.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::archive::{ArchiveLimits, normalise, verify_archive};
use crate::engine::{Outcome, UpdateEngine};
use crate::error::{UpdaterError, UpdaterErrorCode};
use crate::journal::Step;
use crate::testing::FakeFileSystem;

use super::fixtures::{self, Fixture, layout, make_zip, manifest_for, new_version, old_version};

/// Too little free space refuses before anything is unpacked.
#[test]
fn free_space_below_the_factor_refuses_before_unpacking() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::good();

    // Exactly one payload's worth free, but the policy wants two.
    fs.disk_full_at(fs.used_bytes() + fixture.payload.len() as u64);

    assert_eq!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Refused(UpdaterErrorCode::DiskSpace)
    );
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
    assert!(!fs.has_dir(&layout.staging_path(&new_version())));
    assert!(!fs.has_dir(&layout.version_dir(&new_version())));
    assert_eq!(service.running(), Some(old_version()));
}

/// A disk that runs dry while unpacking rolls back, and `current` never moves.
#[test]
fn disk_full_during_unpack_leaves_no_current_change() {
    let fs = FakeFileSystem::new();
    let layout = layout();
    fixtures::seed_installed(&fs, &layout);
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    // A large file the small disk cannot hold, while the pre-unpack guard sees plenty of room
    // (the free-space report and the write cap are decoupled).
    let big = vec![7_u8; 100_000];
    let mut fixture = Fixture::with_files(&[("bin/spot", big.as_slice())]);
    // The file is highly compressible; raise the bomb ratio so this test isolates disk-full.
    fixture.limits.max_compression_ratio = 1_000_000;
    fs.report_free_space(1_000_000_000);
    fs.disk_full_at(fs.used_bytes() + 5_000);

    match engine.apply(&fixture.request(), fixtures::NOW) {
        Outcome::RolledBack { failed_step, .. } => assert_eq!(failed_step, Step::Unpack),
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(fs.pointer(&layout.current_path()), Some(old_version()));
    assert!(!fs.has_dir(&layout.staging_path(&new_version())));
    assert_eq!(service.running(), Some(old_version()));
}

/// An entry that would escape the staging directory is refused.
#[test]
fn an_archive_entry_that_escapes_is_refused() {
    let archive = make_zip(&[("../evil", b"payload")]);
    let manifest = manifest_for(
        &new_version(),
        2,
        Vec::new(),
        &[("evil", b"payload")],
        &archive,
    );
    let error = verify_archive(&manifest, &archive, &ArchiveLimits::default()).unwrap_err();
    assert_eq!(error.code(), UpdaterErrorCode::ArchiveEscape);
}

/// An absolute entry name is refused too.
#[test]
fn an_absolute_archive_entry_is_refused() {
    let archive = make_zip(&[("/etc/passwd", b"payload")]);
    let manifest = manifest_for(
        &new_version(),
        2,
        Vec::new(),
        &[("passwd", b"payload")],
        &archive,
    );
    let error = verify_archive(&manifest, &archive, &ArchiveLimits::default()).unwrap_err();
    assert_eq!(error.code(), UpdaterErrorCode::ArchiveEscape);
}

/// A zip bomb is refused while it is being read.
#[test]
fn a_zip_bomb_is_refused() {
    let bomb = vec![0_u8; 4 * 1024 * 1024];
    let archive = make_zip(&[("big", &bomb)]);
    let manifest = manifest_for(
        &new_version(),
        2,
        Vec::new(),
        &[("big", bomb.as_slice())],
        &archive,
    );
    let error = verify_archive(&manifest, &archive, &ArchiveLimits::default()).unwrap_err();
    assert_eq!(error.code(), UpdaterErrorCode::ArchiveBomb);
}

/// A file the manifest does not list is refused.
#[test]
fn an_unlisted_entry_is_refused() {
    let archive = make_zip(&[("listed", b"one"), ("extra", b"two")]);
    let manifest = manifest_for(
        &new_version(),
        2,
        Vec::new(),
        &[("listed", b"one")],
        &archive,
    );
    let error = verify_archive(&manifest, &archive, &ArchiveLimits::default()).unwrap_err();
    assert_eq!(error.code(), UpdaterErrorCode::ArchiveUnlisted);
}

/// A file the manifest lists but the archive lacks is refused.
#[test]
fn a_missing_entry_is_refused() {
    let archive = make_zip(&[("a", b"one")]);
    let manifest = manifest_for(
        &new_version(),
        2,
        Vec::new(),
        &[("a", b"one"), ("b", b"two")],
        &archive,
    );
    let error = verify_archive(&manifest, &archive, &ArchiveLimits::default()).unwrap_err();
    assert_eq!(error.code(), UpdaterErrorCode::ArchiveMissing);
}

/// Path safety collapses dots and refuses every escape.
#[test]
fn path_safety_collapses_and_refuses() {
    assert_eq!(normalise("a/./b").unwrap(), "a/b");
    assert_eq!(normalise("a//b").unwrap(), "a/b");
    assert_eq!(normalise("a/b/").unwrap(), "a/b");
    for bad in ["../x", "/x", "a\\b", "C:x", "a\0b", "", ".", "a/.."] {
        assert!(
            matches!(normalise(bad), Err(UpdaterError::ArchiveEscape { .. })),
            "{bad:?} must be refused"
        );
    }
}

#[test]
fn directory_entries_must_also_be_path_safe() {
    use std::io::Cursor;
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_directory("../escape/", zip::write::SimpleFileOptions::default())
        .unwrap();
    let archive = writer.finish().unwrap().into_inner();
    let manifest = manifest_for(&new_version(), 2, Vec::new(), &[], &archive);
    assert_eq!(
        verify_archive(&manifest, &archive, &ArchiveLimits::default())
            .unwrap_err()
            .code(),
        UpdaterErrorCode::ArchiveEscape
    );
}

#[test]
fn normalized_and_case_aliases_are_not_independent_files() {
    for names in [["bin/spot", "bin/./spot"], ["bin/spot", "BIN/SPOT"]] {
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .map(|name| (*name, b"same".as_slice()))
            .collect();
        let archive = make_zip(&files);
        let manifest = manifest_for(&new_version(), 2, Vec::new(), &files, &archive);
        assert_eq!(
            verify_archive(&manifest, &archive, &ArchiveLimits::default())
                .unwrap_err()
                .code(),
            UpdaterErrorCode::Archive
        );
        let manifest = manifest_for(&new_version(), 2, Vec::new(), &files[..1], &archive);
        assert_eq!(
            verify_archive(&manifest, &archive, &ArchiveLimits::default())
                .unwrap_err()
                .code(),
            UpdaterErrorCode::Archive
        );
    }
}

#[test]
fn free_space_arithmetic_never_understates_a_large_ratio() {
    let policy = crate::UpdatePolicy {
        free_space_num: 2,
        free_space_den: 2,
        ..crate::UpdatePolicy::default()
    };
    assert_eq!(crate::required_free_space(u64::MAX, &policy), u64::MAX);
    let invalid = crate::UpdatePolicy {
        free_space_den: 0,
        ..policy
    };
    assert_eq!(crate::required_free_space(1, &invalid), u64::MAX);
}
