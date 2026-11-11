//! The command line (WP-DX-01.T1 deliverable 6): exit codes 0, 2 and 3, and the `--dry-run`
//! that writes nothing.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use clap::Parser;
use spot_scaffold::cli::{Cli, run};

#[test]
fn usage_errors_exit_two() {
    let error = Cli::try_parse_from(["spot-scaffold", "app", "new"]).unwrap_err();
    assert_eq!(error.exit_code(), 2);
}

#[test]
fn a_dry_run_writes_nothing_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::try_parse_from([
        "spot-scaffold",
        "app",
        "new",
        "acme",
        "--dry-run",
        "--out",
        dir.path().to_str().unwrap(),
    ])
    .unwrap();
    assert_eq!(run(&cli), 0);
    assert!(!dir.path().join("app").exists(), "dry run wrote files");
}

#[test]
fn a_refusal_exits_three() {
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::try_parse_from([
        "spot-scaffold",
        "app",
        "new",
        "Sales",
        "--out",
        dir.path().to_str().unwrap(),
    ])
    .unwrap();
    assert_eq!(run(&cli), 3);
}

#[test]
fn a_full_run_writes_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::try_parse_from([
        "spot-scaffold",
        "app",
        "new",
        "acme",
        "--with-controller",
        "--with-script",
        "--out",
        dir.path().to_str().unwrap(),
    ])
    .unwrap();
    assert_eq!(run(&cli), 0);
    assert!(dir.path().join("app/manifest.toml").exists());
    assert!(dir.path().join("app/controllers.toml").exists());
}

fn generate(extra: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut args = vec!["spot-scaffold", "app", "new", "acme"];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--out", dir.path().to_str().unwrap()]);
    let cli = Cli::try_parse_from(args).unwrap();
    assert_eq!(run(&cli), 0);
    dir
}

fn files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn readme_of(dir: &tempfile::TempDir) -> String {
    let mut files = Vec::new();
    files_under(dir.path(), &mut files);
    let readme = files
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "README.md"))
        .expect("a generated README");
    std::fs::read_to_string(readme).unwrap()
}

#[test]
fn no_licence_flag_records_no_licence_anywhere() {
    let dir = generate(&["--with-controller", "--with-script", "--example-fields"]);
    let readme = readme_of(&dir);
    assert!(
        readme.contains(
            "Author: Spotledger. Licence: not chosen yet; add a LICENSE file and set the licence here."
        ),
        "{readme}"
    );
    let mut files = Vec::new();
    files_under(dir.path(), &mut files);
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        assert!(
            !text.contains("Proprietary"),
            "{} claims a licence",
            file.display()
        );
    }
}

#[test]
fn a_chosen_licence_is_recorded() {
    for id in ["Apache-2.0", "MIT"] {
        let dir = generate(&["--licence", id]);
        let readme = readme_of(&dir);
        assert!(readme.contains(&format!("Licence: {id}.")), "{readme}");
        assert!(!readme.contains("not chosen yet"), "{readme}");
    }
}
