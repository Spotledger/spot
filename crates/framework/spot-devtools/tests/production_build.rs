//! Devtools are compiled out of production builds.
#![allow(clippy::unwrap_used, clippy::expect_used)]
//!
//! The debug server's entry points sit behind the `dev-server` feature. This test fails when the
//! feature is on by default in this crate, or when a manifest in the workspace turns it on for a
//! normal dependency: `spot-cli`'s release build, the shipped binary, must never carry it. Only a
//! `[dev-dependencies]` entry (test builds) may enable it, and `spot-cli` may not even do that.

use std::path::{Path, PathBuf};

const FEATURE: &str = "dev-server";
const CRATE: &str = "spot-devtools";

/// The manifests that reach `spot-devtools` with the feature on, as `(section, line)` pairs, from
/// one manifest's text.
fn enabling_lines(manifest: &str) -> Vec<(String, String)> {
    let mut section = String::new();
    let mut found = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.to_owned();
            continue;
        }
        let names_crate = trimmed.contains(CRATE);
        let names_feature = trimmed.contains(FEATURE);
        // `spot-devtools = { ..., features = ["dev-server"] }`, or a feature that forwards:
        // `x = ["spot-devtools/dev-server"]`.
        if names_crate && names_feature {
            found.push((section.clone(), trimmed.to_owned()));
        }
        // A `[dependencies.spot-devtools]` table with a `features = [...]` line.
        if section.contains(CRATE) && trimmed.starts_with("features") && names_feature {
            found.push((section.clone(), trimmed.to_owned()));
        }
    }
    found
}

fn is_dev_only(section: &str) -> bool {
    section.contains("dev-dependencies")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn manifests(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let manifest = dir.join("Cargo.toml");
    if manifest.is_file() {
        out.push(manifest);
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() && !name.starts_with('.') && name != "target" && name != "node_modules" {
            manifests(&path, depth - 1, out);
        }
    }
}

#[test]
fn the_feature_is_off_by_default() {
    let own = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("own manifest");
    let mut in_features = false;
    let mut default_line = None;
    for line in own.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_features = trimmed == "[features]";
        } else if in_features && trimmed.starts_with("default") {
            default_line = Some(trimmed.to_owned());
        }
    }
    let default_line = default_line.expect("a `default` feature line");
    assert!(!default_line.contains(FEATURE), "{default_line}");
}

#[test]
fn no_production_manifest_enables_the_feature() {
    let root = workspace_root();
    let mut all = Vec::new();
    manifests(&root, 4, &mut all);
    assert!(
        all.iter().any(|path| path.ends_with("spot-cli/Cargo.toml")),
        "spot-cli's manifest was not found under {}",
        root.display()
    );
    for path in all {
        let own = path.ends_with("spot-devtools/Cargo.toml");
        let cli = path.ends_with("spot-cli/Cargo.toml");
        let text = std::fs::read_to_string(&path).expect("manifest");
        for (section, line) in enabling_lines(&text) {
            if own {
                continue;
            }
            assert!(
                !cli,
                "spot-cli must not enable `{FEATURE}` at all ({section}: {line})"
            );
            assert!(
                is_dev_only(&section),
                "{} enables `{FEATURE}` outside [dev-dependencies] ({section}: {line})",
                path.display()
            );
        }
    }
}

#[test]
fn the_check_catches_a_production_enable() {
    let production = "[dependencies]\nspot-devtools = { path = \"../spot-devtools\", features = [\"dev-server\"] }\n";
    let found = enabling_lines(production);
    assert_eq!(found.len(), 1);
    assert!(!is_dev_only(&found[0].0));

    let forwarded = "[features]\ndefault = [\"spot-devtools/dev-server\"]\n";
    assert!(!is_dev_only(&enabling_lines(forwarded)[0].0));

    let table = "[dependencies.spot-devtools]\npath = \"x\"\nfeatures = [\"dev-server\"]\n";
    assert!(!is_dev_only(&enabling_lines(table)[0].0));

    let dev = "[dev-dependencies]\nspot-devtools = { path = \"x\", features = [\"dev-server\"] }\n";
    assert!(is_dev_only(&enabling_lines(dev)[0].0));

    assert!(enabling_lines("[dependencies]\nspot-devtools = { path = \"x\" }\n").is_empty());
}
