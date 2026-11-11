//! A guard must never turn a caller-chosen command id into a request context. The rule is
//! enforced by shape: [`CaseContext::parse`] stays for the engine's own use, and only
//! [`RequestAuthority::authenticate`] may decide a request context is real. This test scans the
//! crate's own sources, the cheap guard the repository uses for such rules (see
//! `spot-bpm-documents/tests/guard.rs`), so a future edit that reintroduces the shortcut fails
//! here and the PR conversation explains why the rule changed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn no_guard_reads_a_request_context_from_a_command_id() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_sources(&root);
    assert!(files.len() >= 8, "the crate's sources were found");

    for file in files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("from_invocation"),
            "{}: a guard must not parse a command id into a request context; \
             ask RequestAuthority::authenticate instead",
            file.display()
        );
        if name != "context.rs" {
            assert!(
                !text.contains("CaseContext::parse"),
                "{}: CaseContext::parse is the engine's plain parser; a guard must not use it \
                 on a caller-supplied command id",
                file.display()
            );
        }
    }
}

#[test]
fn the_controller_authenticates_the_command_id() {
    // The one place a command id becomes a context is the guard's `authenticate`, which the
    // controller calls. This pins that wiring so the decision cannot quietly fall back to a
    // plain parse.
    let controller =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/controller.rs"))
            .unwrap();
    assert!(
        controller.contains("authenticate("),
        "the controller resolves the request context through the authority"
    );
    let guard =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/guard.rs")).unwrap();
    assert!(
        guard.contains("fn authenticate("),
        "the guard exposes the authority's authenticate"
    );
}
