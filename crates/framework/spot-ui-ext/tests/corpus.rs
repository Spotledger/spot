//! The shared corpus runner: every valid example of `contracts/ui-ext/examples/` validates, and

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use spot_ui_ext::{UiExtError, validate};

fn findings(path: &Path) -> (String, Vec<UiExtError>) {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let contribution = support::sample_contribution(vec![(name.clone(), text)]);
    (name, validate(&contribution))
}

fn json_files(directory: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(directory)
        .unwrap_or_else(|e| panic!("{}: {e}", directory.display()))
        .map(|entry| entry.unwrap_or_else(|e| panic!("{e}")).path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    files
}

#[test]
fn every_valid_example_validates() {
    let examples = support::contracts_dir().join("examples");
    let mut files = json_files(&examples.join("valid"));
    files.push(examples.join("sample-app").join("ui.json"));
    assert!(files.len() >= 8, "the corpus has valid examples");
    for file in files {
        let (name, findings) = findings(&file);
        assert!(findings.is_empty(), "{name} should validate: {findings:?}");
    }
}

#[test]
fn every_invalid_example_fails_with_exactly_its_code() {
    let files = json_files(&support::contracts_dir().join("examples/invalid"));
    assert!(files.len() >= 26, "one example per code");
    for file in files {
        let (name, found) = findings(&file);
        let text = fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let document: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let comment = document
            .get("$comment")
            .and_then(|value| value.as_str())
            .unwrap_or_else(|| panic!("{name}: no $comment"));
        let expected = comment
            .strip_prefix("expect: ")
            .and_then(|rest| rest.split_whitespace().next())
            .map(|word| word.trim_end_matches('.'))
            .unwrap_or_else(|| panic!("{name}: $comment must start `expect: <code>`"));
        let codes: BTreeSet<&str> = found.iter().map(|finding| finding.code).collect();
        assert_eq!(
            codes,
            BTreeSet::from([expected]),
            "{name} should fail with exactly {expected}: {found:?}"
        );
        for finding in &found {
            assert_eq!(finding.app.as_deref(), Some("sample"), "{name}: {finding}");
            assert_eq!(finding.file, name, "{name}: {finding}");
            assert!(
                !finding.pointer.is_empty() || finding.line.is_some(),
                "{name}: {finding} has neither a pointer nor a position"
            );
            assert!(!finding.message.is_empty(), "{name}: {finding}");
        }
    }
}
