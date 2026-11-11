//! The real filesystem over `std::fs`, exercised on a temporary directory.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::fs::{FileSystem, StdFileSystem};

#[test]
fn std_filesystem_round_trips_on_a_temp_dir() {
    let dir = tempfile::tempdir().unwrap();
    let fs = StdFileSystem::new();
    let root = dir.path();

    let file = root.join("a.txt");
    fs.write_atomic(&file, b"hello").unwrap();
    assert_eq!(fs.read(&file).unwrap(), b"hello");
    assert!(fs.exists(&file).unwrap());

    // A durable write replaces the bytes and leaves no temporary file behind.
    fs.durable_write(&file, b"world").unwrap();
    assert_eq!(fs.read(&file).unwrap(), b"world");

    let sub = root.join("sub");
    fs.create_dir_all(&sub).unwrap();
    let moved = sub.join("b.txt");
    fs.rename(&file, &moved).unwrap();
    assert!(!fs.exists(&file).unwrap());
    assert_eq!(fs.read(&moved).unwrap(), b"world");
    assert_eq!(fs.list(&sub).unwrap(), vec!["b.txt".to_owned()]);

    // No stray temporary file from the atomic write.
    assert!(
        !fs.list(root)
            .unwrap()
            .iter()
            .any(|name| name.contains(".tmp-"))
    );

    #[cfg(unix)]
    assert!(fs.free_space(root).unwrap() > 0);

    fs.remove_file(&moved).unwrap();
    fs.remove_dir_all(&sub).unwrap();
    assert!(!fs.exists(&sub).unwrap());
    // Removing an absent directory is fine.
    fs.remove_dir_all(&sub).unwrap();
}

#[test]
fn a_real_disk_update_and_probation_rollback_preserve_data() {
    use super::fixtures::{self, Fixture, OLD_FILES, new_version, old_version};
    use crate::{Layout, Outcome, UpdateEngine};
    let temp = tempfile::tempdir().unwrap();
    let layout = Layout::new(temp.path());
    let fs = StdFileSystem::new();
    for (name, bytes) in OLD_FILES {
        let path = layout.version_file(&old_version(), name);
        fs.create_dir_all(path.parent().unwrap()).unwrap();
        fs.durable_write(&path, bytes).unwrap();
    }
    fs.durable_write(&layout.current_path(), b"1.0.0").unwrap();
    let data = layout.root().join("data");
    fs.create_dir_all(&data).unwrap();
    fs.durable_write(&data.join("database"), b"user-data")
        .unwrap();
    let service = fixtures::running_service();
    let hooks = fixtures::hooks();
    let engine = UpdateEngine::new(&fs, &service, &hooks, layout.clone());
    let fixture = Fixture::with_schema_change();
    assert!(matches!(
        engine.apply(&fixture.request(), fixtures::NOW),
        Outcome::Applied { .. }
    ));
    assert_eq!(engine.installed_version().unwrap(), Some(new_version()));
    assert_eq!(hooks.epoch(), 3);
    service.crash();
    assert!(matches!(
        engine.tick(fixtures::NOW + 1),
        Outcome::RolledBack { .. }
    ));
    assert_eq!(engine.installed_version().unwrap(), Some(old_version()));
    assert_eq!(service.running(), Some(old_version()));
    assert_eq!(hooks.epoch(), 2);
    assert_eq!(fs.read(&data.join("database")).unwrap(), b"user-data");
    assert!(!fs.exists(&layout.journal_path()).unwrap());
    assert!(!fs.exists(&layout.version_dir(&new_version())).unwrap());
}
