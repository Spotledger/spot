//! The committed fixture must be exactly what the generator produces.
//!
//! The fixture is old journal bytes: once committed, nothing may quietly rewrite them, because a
//! fixture that follows the code proves nothing — it would rebuild whenever the format changes
//! and the test would stay green while every journal written by the previous release became
//! unreadable. So the generator is asked to produce the fixture again and its bytes are compared
//! with the committed files, byte for byte and file for file. A format change that alters old
//! bytes turns this test red; regenerating the fixture is then a deliberate decision, made in a
//! commit that says so (see `fixtures/README.md`).
//!
//! The comparison covers the set of files too: an object that the generator no longer writes may
//! not linger in the fixture, and an object it does write may not be missing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use spot_journal_compat::{FixtureKeys, KEY_ID, default_fixture_dir, generator};

/// Every file under `root`, as `(path, bytes)` with `/` separators, skipping the hand-written
/// `README.md`.
fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut found = BTreeMap::new();
    collect(root, root, &mut found);
    found
}

fn collect(root: &Path, dir: &Path, found: &mut BTreeMap<String, Vec<u8>>) {
    for entry in std::fs::read_dir(dir).expect("the fixture directory is readable") {
        let path: PathBuf = entry.expect("a directory entry").path();
        if path.is_dir() {
            collect(root, &path, found);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("a fixture file is below the fixture directory")
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<String>>()
            .join("/");
        if relative == "README.md" {
            continue;
        }
        found.insert(
            relative,
            std::fs::read(&path).expect("a fixture file is readable"),
        );
    }
}

#[test]
fn the_generator_reproduces_the_committed_bytes_exactly() {
    let generated = generator::build().expect("the fixture generates");
    let committed = files(&default_fixture_dir());

    let generated_paths: Vec<&String> = generated.files.iter().map(|(path, _)| path).collect();
    for path in &generated_paths {
        let bytes = committed
            .get(*path)
            .unwrap_or_else(|| panic!("{path} is generated but not committed"));
        assert_eq!(
            generated.file(path),
            Some(bytes.as_slice()),
            "{path} differs from what the generator produces: the committed journal bytes changed"
        );
    }
    let committed_paths: Vec<&String> = committed.keys().collect();
    assert_eq!(
        generated_paths, committed_paths,
        "the committed fixture holds exactly the files the generator writes"
    );
}

#[test]
fn the_committed_public_key_is_the_test_only_keys_half() {
    // `key.pub` is the public half of the TEST-ONLY constant in `src/generator/key.rs`, and the
    // fixture's key id is the one every object names. If the seed ever changes, the fixture stops
    // verifying, which is what this test says out loud.
    let keys = FixtureKeys::from_hex(
        &std::fs::read_to_string(default_fixture_dir().join("key.pub"))
            .expect("the fixture has a key.pub"),
    )
    .expect("key.pub is a public key");
    let signer = generator::key::FixtureSigner::new();
    assert_eq!(keys.to_hex(), signer.keys().to_hex());
    assert_eq!(KEY_ID, "journal-fixture-key-1");
}
