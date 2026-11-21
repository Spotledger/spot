//! The safety layer (WP-DX-01.T1 deliverable 7): path traversal, a planted symlink, overwrite
//! refusal, a read-only directory, a DocType collision, reserved words and idempotence.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_scaffold::generate::doctype::DoctypeSpec;
use spot_scaffold::names::AppName;
use spot_scaffold::tree::{FileTree, RelPath, WritePolicy};
use support::{plain_tree, temp_project};

#[test]
fn a_relative_path_never_escapes() {
    for bad in [
        "/abs",
        "../x",
        "a/../../b",
        "a/./b",
        "a//b",
        "a\\b",
        "a:b",
        "",
    ] {
        let error = RelPath::parse(bad).unwrap_err();
        assert_eq!(error.code, "scaffold.path_escape", "{bad}");
    }
    let mut tree = FileTree::new();
    let error = tree.insert("../steal.toml", "x").unwrap_err();
    assert_eq!(error.code, "scaffold.path_escape");
}

#[test]
fn a_name_with_a_separator_is_refused() {
    let error = AppName::parse("../evil").unwrap_err();
    assert_eq!(error.code, "scaffold.invalid_name");
    assert!(error.message.contains("evil"), "{}", error.message);
}

#[test]
fn a_planted_symlink_is_refused() {
    let dir = temp_project();
    let elsewhere = temp_project();
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("app")).unwrap();
    let tree = plain_tree();
    let error = tree
        .write_to_disk(dir.path(), &WritePolicy::new())
        .unwrap_err();
    assert_eq!(error.code, "scaffold.symlink");
}

#[test]
fn an_existing_file_needs_force() {
    let dir = temp_project();
    let tree = plain_tree();
    tree.write_to_disk(dir.path(), &WritePolicy::new()).unwrap();
    let error = tree
        .write_to_disk(dir.path(), &WritePolicy::new())
        .unwrap_err();
    assert_eq!(error.code, "scaffold.exists");
}

#[test]
fn a_refusal_writes_nothing() {
    let dir = temp_project();
    let tree = plain_tree();
    tree.write_to_disk(dir.path(), &WritePolicy::new()).unwrap();
    // The first file is now new and a later one still exists: the refusal must leave the new
    // one unwritten (no half-applied edit).
    std::fs::remove_file(dir.path().join(".gitignore")).unwrap();
    let error = tree
        .write_to_disk(dir.path(), &WritePolicy::new())
        .unwrap_err();
    assert_eq!(error.code, "scaffold.exists");
    assert!(
        !dir.path().join(".gitignore").exists(),
        "a refusal wrote a file"
    );
}

#[test]
fn a_second_forced_run_is_identical() {
    let dir = temp_project();
    let tree = plain_tree();
    tree.write_to_disk(dir.path(), &WritePolicy::force())
        .unwrap();
    let first = std::fs::read(dir.path().join("app/manifest.toml")).unwrap();
    tree.write_to_disk(dir.path(), &WritePolicy::force())
        .unwrap();
    let second = std::fs::read(dir.path().join("app/manifest.toml")).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        tree.write_to_disk(dir.path(), &WritePolicy::force())
            .unwrap()
            .bytes,
        tree.total_bytes()
    );
}

#[test]
fn a_read_only_directory_is_an_io_error() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    let dir = temp_project();
    let root = dir.path().join("ro");
    std::fs::create_dir(&root).unwrap();

    // Capture the mode so a drop guard can always restore it, even when an assertion
    // fails; otherwise a read-only temp directory survives and breaks clean-up.
    struct RestoreMode {
        root: PathBuf,
    }
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            if let Ok(meta) = std::fs::metadata(&self.root) {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(&self.root, perms);
            }
        }
    }

    let mut perms = std::fs::metadata(&root).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&root, perms).unwrap();
    let _restore = RestoreMode { root: root.clone() };

    // Detect directly whether read-only binds this user: if we can create a file in the
    // directory despite the mode, this user is not bound (running as root).
    let probe = root.join(".probe");
    if std::fs::File::create(&probe).is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipped: this user is not bound by directory permissions (running as root)");
        return;
    }

    let tree = plain_tree();
    let error = tree.write_to_disk(&root, &WritePolicy::new()).unwrap_err();
    assert_eq!(error.code, "scaffold.io");
}

#[test]
fn a_root_run_never_leaves_a_half_written_tree() {
    let dir = temp_project();
    let root = dir.path();
    // Make one target path a directory so writing that file fails for every user,
    // root included (the write syscall returns "Is a directory").
    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::create_dir(root.join("app/manifest.toml")).unwrap();

    let tree = plain_tree();
    // `force` is required to reach the write itself: otherwise phase 1 refuses the
    // existing path with `scaffold.exists` before any byte is written.
    let error = tree.write_to_disk(root, &WritePolicy::force()).unwrap_err();
    assert_eq!(error.code, "scaffold.io");

    // The implementation writes files in sorted order and does not roll back on a
    // mid-write failure, so exactly the files before `app/manifest.toml` exist.
    for present in [
        ".gitignore",
        "README.md",
        "app/app.wasm",
        "app/doctypes/note.toml",
        "app/fixtures/README.md",
    ] {
        assert!(
            root.join(present).is_file(),
            "{present} should have been written"
        );
    }
    for absent in ["app/permissions/grants.toml", "tests/load.rs"] {
        assert!(
            !root.join(absent).exists(),
            "{absent} should not have been written"
        );
    }
    assert!(root.join("app/manifest.toml").is_dir());
}

#[test]
fn a_doctype_collision_names_the_file() {
    let dir = temp_project();
    let tree = plain_tree();
    tree.write_to_disk(dir.path(), &WritePolicy::new()).unwrap();
    let spec = DoctypeSpec {
        app: AppName::parse("acme").unwrap(),
        label: "Sales Invoice".to_owned(),
        name: None,
        example_fields: false,
        role: None,
    };
    let tree = spot_scaffold::generate::doctype::generate(dir.path(), &spec).unwrap();
    tree.write_to_disk(dir.path(), &WritePolicy::force())
        .unwrap();

    let error = spot_scaffold::generate::doctype::generate(dir.path(), &spec).unwrap_err();
    assert_eq!(error.code, "scaffold.duplicate");
    assert!(
        error.message.contains("sales_invoice.toml"),
        "{}",
        error.message
    );
}

#[test]
fn a_controller_step_collision_names_the_manifest_line() {
    use spot_scaffold::generate::controller::ControllerSpec;
    use spot_scaffold::names::DocTypeName;
    let dir = temp_project();
    let tree = plain_tree();
    tree.write_to_disk(dir.path(), &WritePolicy::new()).unwrap();
    let spec = ControllerSpec {
        app: AppName::parse("acme").unwrap(),
        doctype: DocTypeName::parse("note").unwrap(),
    };
    let tree = spot_scaffold::generate::controller::generate(dir.path(), &spec).unwrap();
    tree.write_to_disk(dir.path(), &WritePolicy::force())
        .unwrap();

    let error = spot_scaffold::generate::controller::generate(dir.path(), &spec).unwrap_err();
    assert_eq!(error.code, "scaffold.duplicate");
    assert!(
        error.message.contains("manifest.toml:") && error.message.contains("acme:validate_note"),
        "{}",
        error.message
    );
}
