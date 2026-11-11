//! Source scan: the execution core never invokes a shell or a process. Privileged work
//! lives behind the injected `OperationHandler`; this crate only ever decides.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

/// Tokens that would mean the crate spawns or talks to a process.
const FORBIDDEN: &[&str] = &[
    "std::process",
    "process::Command",
    "Command::new(\"sh\"",
    "Command::new(\"cmd\"",
    "sh -c",
    "powershell",
    "cmd.exe",
    ".spawn()",
];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_crate_never_spawns_a_shell_or_process() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    assert!(files.len() >= 10, "the scan sees the crate's modules");
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        for token in FORBIDDEN {
            assert!(
                !text.contains(token),
                "{} contains the forbidden token {token:?}",
                file.display()
            );
        }
    }
}
