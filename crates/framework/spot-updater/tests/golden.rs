//! Golden vectors: the canonical bytes and the signature of a fixed manifest, byte for byte. A
//! change to the format or the encoding fails here, loudly.
//!
//! The values were captured from this implementation at format version 1 (the first release of
//! the update layer). They are pinned on purpose: changing any of them changes what existing
//! signatures mean, so it needs a new domain prefix and format version, not an edit here.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_updater::{Channel, UpdateManifest};

#[test]
fn the_key_id_is_pinned() {
    assert_eq!(
        key(SIGNER).public().key_id().as_str(),
        "34750f98bd59fcfc946da45aaabe933b"
    );
}

// Captured from this implementation (not an independent reference).
#[test]
fn the_canonical_bytes_are_pinned() {
    assert_eq!(
        String::from_utf8(manifest().canonical_bytes()).unwrap(),
        r#"{"channel":"stable","files":[{"path":"bin/spotledger.exe","sha256":"37456ce54a2ef39b6c9c1d96ddc978f2edc730744bd2c9872dc1cc9ac886b00e","size":14}],"format":"spot-update-manifest","migrations":[],"min_from":"1.0.0","notes":"Bug fixes","payload":{"file":"spotledger-2.0.0.zip","sha256":"2bd52e0dc8577a407d64041aab65c127b37c39852f226152d34fb6f6e520ab63","size":31},"product":"spotledger","released_at":1700000000,"requires_os":"Windows 10","schema_epoch":3,"version":"2.0.0","via":[]}"#
    );
}

// Captured from this implementation (not an independent reference).
#[test]
fn the_signature_is_pinned() {
    assert_eq!(
        spot_updater::canonical::b64(sign(&manifest(), SIGNER).signature()),
        "pNrGgaIqstHeF4PQr1J9+AkevLP8meDc8lHzdEd1XkSKQvqLirk1sXxxGQZbFbVZ9SBSbyWfuin0gG8dH8jeBg=="
    );
}

#[test]
fn a_manifest_round_trips_through_toml() {
    let text = r#"
format = "spot-update-manifest"
product = "spotledger"
channel = "beta"
version = "2.1.0"
min_from = "1.5.0"
via = ["1.8.0", "2.0.0"]
released_at = 1700000000
schema_epoch = 4
migrations = [3]
requires_os = "Windows 11"
notes = "Fixes"
files = [{ path = "bin/spotledger.exe", size = 14, sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }]

[payload]
file = "spotledger-2.1.0.zip"
size = 30
sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#;
    let manifest = UpdateManifest::from_toml(text).unwrap();
    assert_eq!(manifest.product, PRODUCT);
    assert_eq!(manifest.channel, Channel::Beta);
    assert_eq!(manifest.version, semver::Version::new(2, 1, 0));
    assert_eq!(manifest.min_from, semver::Version::new(1, 5, 0));
    assert_eq!(
        manifest.via,
        vec![semver::Version::new(1, 8, 0), semver::Version::new(2, 0, 0)]
    );
    assert_eq!(manifest.migrations, vec![3]);
    assert_eq!(manifest.payload.file, "spotledger-2.1.0.zip");
    assert_eq!(manifest.schema_epoch, 4);
    assert_eq!(manifest.files.len(), 1);
    assert_eq!(manifest.requires_os.as_deref(), Some("Windows 11"));
}

#[test]
fn an_unknown_field_is_refused() {
    let text = r#"
format = "spot-update-manifest"
product = "spotledger"
channel = "stable"
version = "2.0.0"
min_from = "1.0.0"
released_at = 1700000000
schema_epoch = 3
surprise = true

[payload]
file = "a.zip"
size = 1
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;
    let error = UpdateManifest::from_toml(text).unwrap_err();
    assert_eq!(error.code().as_str(), "updater.manifest");
}
