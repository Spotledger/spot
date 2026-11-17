//! The usage-manifest generator over a small platform root (WP-STO-07.T9): no database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
use spot_storage_contract::RecordId;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A temporary platform root with the platform app and a private copy of the sample app.
struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn root() -> Root {
    let dir =
        Root(std::env::temp_dir().join(format!("spot_usage_manifest_{}", RecordId::new_v7())));
    copy_dir(&repo().join("apps/sample"), &dir.0.join("sample"));
    std::fs::write(
        dir.0.join("common.toml"),
        format!(
            "apps = [{:?}, 'sample']\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\n\
             admin_url = 'postgres://u:p@127.0.0.1:15432/postgres'\n",
            repo().join("apps/platform").display().to_string(),
        ),
    )
    .unwrap();
    dir
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_usage_manifest"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn generate(root: &Root, out: &Path) -> Output {
    run(&[
        "generate",
        "--root",
        root.0.to_str().unwrap(),
        "--release",
        "2026.11.0",
        "--out",
        out.to_str().unwrap(),
    ])
}

fn check(root: &Root, manifest: &Path) -> Output {
    run(&[
        "check",
        "--root",
        root.0.to_str().unwrap(),
        "--manifest",
        manifest.to_str().unwrap(),
    ])
}

#[test]
fn generate_lists_a_table_a_column_and_the_derived_traversal_index() {
    let root = root();
    let out = root.0.join("release-meta/usage.toml");
    let result = generate(&root, &out);
    assert_eq!(result.status.code(), Some(0), "{}", stdout(&result));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("release = \"2026.11.0\""), "{text}");
    assert!(text.contains("\"task\""), "table missing: {text}");
    assert!(text.contains("\"task.subject\""), "column missing: {text}");
    assert!(
        text.contains("\"index:task.gx_task_project_rev\""),
        "derived traversal index missing: {text}"
    );
    assert!(text.ends_with('\n'));
}

#[test]
fn a_second_generate_is_byte_identical() {
    let root = root();
    let first = root.0.join("first.toml");
    let second = root.0.join("second.toml");
    assert_eq!(generate(&root, &first).status.code(), Some(0));
    assert_eq!(generate(&root, &second).status.code(), Some(0));
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap()
    );
}

#[test]
fn check_passes_on_a_generated_manifest() {
    let root = root();
    let out = root.0.join("usage.toml");
    assert_eq!(generate(&root, &out).status.code(), Some(0));
    let result = check(&root, &out);
    assert_eq!(result.status.code(), Some(0), "{}", stdout(&result));
}

#[test]
fn a_new_column_of_a_listed_table_makes_check_exit_one() {
    let root = root();
    let out = root.0.join("usage.toml");
    assert_eq!(generate(&root, &out).status.code(), Some(0));
    let task = root.0.join("sample/doctypes/task.toml");
    let text = std::fs::read_to_string(&task).unwrap()
        + "\n[[fields]]\nname = \"extra_note\"\nlabel = \"Extra Note\"\ntype = \"text\"\nmax_chars = 64\n";
    std::fs::write(&task, text).unwrap();
    let result = check(&root, &out);
    let printed = stdout(&result);
    assert_eq!(result.status.code(), Some(1), "{printed}");
    assert!(printed.contains("stale-manifest"), "{printed}");
    assert!(printed.contains("task.extra_note"), "{printed}");
    assert!(printed.contains("--release 2026.11.0"), "{printed}");
    assert!(!printed.contains("<id>"), "{printed}");
}

#[test]
fn a_new_table_makes_check_exit_one_with_a_missing_in_manifest_line() {
    let root = root();
    let out = root.0.join("usage.toml");
    assert_eq!(generate(&root, &out).status.code(), Some(0));
    std::fs::write(
        root.0.join("sample/doctypes/extra_note.toml"),
        "name = \"extra_note\"\nlabel = \"Extra Note\"\napp = \"sample\"\nmodule = \"tasks\"\n\n\
         [[fields]]\nname = \"note\"\nlabel = \"Note\"\ntype = \"text\"\nmax_chars = 64\n",
    )
    .unwrap();
    let result = check(&root, &out);
    let printed = stdout(&result);
    assert_eq!(result.status.code(), Some(1), "{printed}");
    assert!(
        printed.contains("missing-in-manifest `extra_note`"),
        "{printed}"
    );
    assert!(printed.contains("regenerate with"), "{printed}");
}

#[test]
fn a_new_index_makes_check_exit_one_with_a_missing_in_manifest_line() {
    let root = root();
    let out = root.0.join("usage.toml");
    assert_eq!(generate(&root, &out).status.code(), Some(0));
    let task = root.0.join("sample/doctypes/task.toml");
    let text = std::fs::read_to_string(&task).unwrap()
        + "\n[[fields]]\nname = \"extra_note\"\nlabel = \"Extra Note\"\ntype = \"text\"\nmax_chars = 64\nindex = true\n";
    std::fs::write(&task, text).unwrap();
    let result = check(&root, &out);
    let printed = stdout(&result);
    assert_eq!(result.status.code(), Some(1), "{printed}");
    assert!(printed.contains("missing-in-manifest"), "{printed}");
    assert!(printed.contains("ix_task_extra_note"), "{printed}");
    assert!(printed.contains("regenerate with"), "{printed}");
}

#[test]
fn a_hand_edited_manifest_naming_an_unknown_column_makes_check_exit_one() {
    let root = root();
    let out = root.0.join("usage.toml");
    assert_eq!(generate(&root, &out).status.code(), Some(0));
    let text = std::fs::read_to_string(&out)
        .unwrap()
        .replace("\"task.subject\"", "\"task.ghost\"");
    std::fs::write(&out, text).unwrap();
    let result = check(&root, &out);
    let printed = stdout(&result);
    assert_eq!(result.status.code(), Some(1), "{printed}");
    assert!(
        printed.contains("unknown-to-catalogue `task.ghost`"),
        "{printed}"
    );
}

#[test]
fn a_missing_manifest_exits_one_and_an_unreadable_root_exits_two() {
    let root = root();
    let missing = root.0.join("nope.toml");
    let result = check(&root, &missing);
    assert_eq!(result.status.code(), Some(1), "{}", stdout(&result));
    assert!(!stdout(&result).contains("<id>"), "{}", stdout(&result));
    let nowhere = Root(root.0.join("does-not-exist"));
    let result = check(&nowhere, &missing);
    assert_eq!(result.status.code(), Some(2));
}
