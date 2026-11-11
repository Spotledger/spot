//! ADR-12: no binary floating point anywhere in this crate. Scans `src/` for the two float type
//! names; a hit fails the build of the tests.
#![allow(missing_docs)]

use std::fs;
use std::path::Path;

fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Whether `text` contains `token` as a whole word (not inside a longer identifier).
fn has_token(text: &str, token: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(token).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + token.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

#[test]
fn the_sources_name_no_float_type() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    assert!(
        !files.is_empty(),
        "no sources found under {}",
        root.display()
    );
    let mut hits = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).unwrap_or_default();
        for token in ["f32", "f64"] {
            // A plain substring check: stricter than whole words, so even `visit_f64` is caught.
            if text.contains(token) {
                hits.push(format!("{}: {token}", file.display()));
            }
        }
    }
    assert!(hits.is_empty(), "float types in src/: {hits:?}");
    // The whole-word helper is exercised so the scan above stays honest about what it checks.
    assert!(has_token("a f64 b", "f64") && !has_token("buf64x", "f64"));
}
