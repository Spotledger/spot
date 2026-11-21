//! Recorded security finding: no adapter in this crate may answer yes to everything. A deployment that wired one
//! in would turn the IAM, telemetry or licence limits off without a trace, so the crate ships
//! only adapters that fail closed (`Unconfigured*`) or do real work, and has no `testing`
//! feature that could leak into a production build through feature unification.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

/// Name prefixes of a stub that grants or ignores everything.
const ALLOW_ALL_PREFIXES: &[&str] = &["AllowAll", "NoLimit", "Unlimited", "Bypass", "PermitAll"];

fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push((
                path.display().to_string(),
                fs::read_to_string(&path).unwrap(),
            ));
        }
    }
}

#[test]
fn no_source_item_is_an_allow_all_adapter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    assert!(!files.is_empty(), "the scan found no sources");
    let mut found = Vec::new();
    for (path, text) in &files {
        for (number, line) in text.lines().enumerate() {
            let line = line.trim_start();
            for keyword in ["struct ", "enum ", "type ", "fn ", "const ", "static "] {
                let Some(position) = line.find(keyword) else {
                    continue;
                };
                let name = &line[position + keyword.len()..];
                if ALLOW_ALL_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
                {
                    found.push(format!("{path}:{}: {line}", number + 1));
                }
            }
        }
    }
    assert!(found.is_empty(), "allow-all adapters found: {found:#?}");
}

#[test]
fn the_crate_has_no_feature_that_could_switch_a_stub_on() {
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    assert!(
        !manifest.lines().any(|l| l.trim() == "[features]"),
        "a features table would let a production build unify a stub in"
    );
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    for (path, text) in &files {
        assert!(
            !text.contains("feature = \"testing\""),
            "{path} gates code behind a `testing` feature"
        );
    }
}
