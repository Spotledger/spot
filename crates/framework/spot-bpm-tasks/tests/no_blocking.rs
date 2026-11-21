//! The access path never blocks a thread on a future: no `block_on`, no poll loop, no poll budget.
//!
//! `Notifier::notify` is async, so `notify.rs` is held to the same rule as every other source.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::fs;
use std::path::Path;

fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push((
                path.display().to_string(),
                fs::read_to_string(&path).unwrap(),
            ));
        }
    }
}

#[test]
fn the_source_has_no_hand_rolled_blocking() {
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(!files.is_empty());
    for (path, text) in files {
        for banned in ["block_on", "POLL_BUDGET", "Waker::noop", "yield_now"] {
            assert!(!text.contains(banned), "{path} contains `{banned}`");
        }
    }
}
