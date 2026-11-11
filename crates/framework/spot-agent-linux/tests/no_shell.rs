//! A source scan: the crate never spawns a shell and never spawns anything outside `real.rs`.
//!
//! The project coding rules forbid building a command line by string concatenation and running anything
//! through a shell. The whole crate reaches the operating system through the [`CommandRunner`]
//! seam; only `real.rs` may name `std::process`. This test fails if a module grows its own
//! process spawn or a shell invocation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

/// Every `.rs` file under `src/`.
fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("the src directory is readable") {
        let path = entry.expect("a readable entry").path();
        if path.is_dir() {
            source_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_shell_and_no_spawn_outside_real_rs() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    source_files(&src, &mut files);
    assert!(!files.is_empty(), "the scan found the sources");

    for file in files {
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let text = std::fs::read_to_string(&file).expect("a readable source file");
        assert!(
            !text.contains("sh -c") && !text.contains("bash -c"),
            "{}: a shell invocation appears",
            file.display()
        );
        if name == "real.rs" {
            continue;
        }
        assert!(
            !text.contains("std::process::Command"),
            "{}: only real.rs may name std::process::Command",
            file.display()
        );
        assert!(
            !text.contains("Command::new("),
            "{}: only real.rs may spawn a process",
            file.display()
        );
    }
}

#[test]
fn the_crate_forbids_unsafe_and_floats() {
    let lib = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .expect("lib.rs is readable");
    assert!(lib.contains("#![forbid(unsafe_code)]"));
    assert!(lib.contains("#![deny(clippy::float_arithmetic)]"));
}
