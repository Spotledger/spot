//! A source scan proving the crate never names a shell and never spawns a process outside
//! the real adapter. The command seam is argv-only; this test is the guard that stays true
//! as modules are added.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}

#[test]
fn no_module_names_a_shell_or_builds_a_shell_string() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = [
        "Command::new(\"sh\"",
        "Command::new(\"bash\"",
        "Command::new(\"/bin/sh\"",
        "Command::new(\"/bin/bash\"",
        ".arg(\"-c\")",
        "\"sh -c\"",
        "\"bash -c\"",
    ];
    for file in rust_files(&src) {
        let text = fs::read_to_string(&file).unwrap();
        for needle in forbidden {
            assert!(
                !text.contains(needle),
                "{} contains {needle:?}",
                file.display()
            );
        }
    }
}

#[test]
fn only_the_real_adapter_spawns_a_process() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in rust_files(&src) {
        if file.file_name().is_some_and(|name| name == "real.rs") {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("std::process"),
            "{} uses std::process outside real.rs",
            file.display()
        );
    }
}

#[test]
fn command_specs_are_built_from_argv_vectors() {
    // The real adapter is the only place `std::process::Command` appears, and it takes the
    // validated argv vector: `command.args(spec.arguments())`.
    let real =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/real.rs")).unwrap();
    assert!(real.contains("command.args(spec.arguments())"));
}
