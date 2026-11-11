//! The compile-level and source guards: the crate cannot write a document outside the lifecycle.
//!
//! 1. `cargo tree` proves no storage backend is a normal dependency, so there is no code path to
//!    a `doc_state` column: the only route to a document is the document engine's public
//!    lifecycle API (`DocumentService`).
//! 2. A source scan proves no code path sets a status field or names the storage column.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The names of the crates under `crates/backends/`.
fn backend_crate_names() -> BTreeSet<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("backends");
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        let manifest = path.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let text = fs::read_to_string(&manifest).unwrap();
        if let Some(name) = manifest_name(&text) {
            names.insert(name);
        }
    }
    assert!(!names.is_empty(), "the backend crates were found");
    names
}

/// The `name = "..."` of a `Cargo.toml`.
fn manifest_name(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return Some(rest.trim().trim_matches('"').to_owned());
            }
        }
    }
    None
}

#[test]
fn no_storage_backend_is_a_normal_dependency() {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "spot-bpm-documents",
            "--edges",
            "normal",
            "--prefix",
            "none",
        ])
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8(output.stdout).unwrap();
    let packages: BTreeSet<&str> = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    for backend in backend_crate_names() {
        assert!(
            !packages.contains(backend.as_str()),
            "the backend `{backend}` must not be a normal dependency"
        );
    }
}

#[test]
fn no_code_path_writes_a_status_field() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_files(&root, &mut files);
    assert!(!files.is_empty(), "the source files were found");
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        for (number, line) in text.lines().enumerate() {
            let code = line.trim();
            // Comments are prose, not a code path.
            if code.starts_with("//") || code.starts_with('*') || code.starts_with("/*") {
                continue;
            }
            for forbidden in ["doc_state", "DocState::", "mark_saved"] {
                assert!(
                    !code.contains(forbidden),
                    "{}:{} names `{forbidden}`",
                    file.display(),
                    number + 1
                );
            }
            assert!(
                !(code.contains(".set(") && code.contains("\"status\"")),
                "{}:{} sets a status field",
                file.display(),
                number + 1
            );
        }
    }
}

/// Every `.rs` file under `root`, recursively.
fn collect_rust_files(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}
