//! Fixtures shared by the spot-pack tests: app directories built in code (no binary
//! fixtures), a valid component assembled from WebAssembly text, the signing material, and
//! the assertions the suites share. Shape follows the harnesses of `spot-bundle` and
//! `spot-bundle-signing`.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;

use spot_bundle_signing::{
    BundleSignature, CertificateChain, KeyPair, PublisherCertificate, RevocationBody,
    RevocationList, RevokedKey, SignedCertificate, TrustAnchor, TrustedRoot, generate_keypair,
    issue_certificate, issue_revocation_list,
};
use spot_pack::CheckReport;

/// The identity of every fixture app: matches the manifest written by [`manifest`].
pub const APP_ID: &str = "test_app";
/// The fixture app's version.
pub const VERSION: &str = "1.0.0";
/// When the fixture signature is made (UTC seconds).
pub const SIGNED_AT: i64 = 150;
/// The check time: comfortably inside every fixture certificate's window.
pub const NOW: i64 = 500;

// --- a fresh sandbox ----------------------------------------------------------------------------

/// A fresh directory under the system temp; every fixture builds inside one, so tests never
/// see each other's files and nothing in the repo is touched.
#[must_use]
pub fn workdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("a temp directory exists");
    let root = tmp.path().to_path_buf();
    (tmp, root)
}

/// Writes an app directory at `dir` from `(rel, bytes)` rows, creating parent directories;
/// returns `dir`. Relative paths are exactly the app-relative names the layout defines.
pub fn write_app(dir: &Path, files: &[(&str, Vec<u8>)]) {
    for (rel, bytes) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parents exist under the temp dir");
        }
        std::fs::write(&path, bytes)
            .unwrap_or_else(|err| panic!("{rel} could not be written: {err}"));
    }
}

// --- the manifest -------------------------------------------------------------------------------

/// The happy-path manifest: one event step, one declared asset, no capabilities. The
/// `capabilities` table is absent on purpose: the component imports only `log`, which the
/// host provides to every app, so nothing needs declaring.
#[must_use]
pub fn manifest() -> String {
    format!(
        "[app]\n\
         name = \"{APP_ID}\"\n\
         version = \"{VERSION}\"\n\
         \n\
         [[events]]\n\
         doctype = \"customer\"\n\
         event = \"before_save\"\n\
         step = \"{APP_ID}:on_save\"\n\
         \n\
         [[assets]]\n\
         path = \"assets/logo.svg\"\n"
    )
}

// --- the component ------------------------------------------------------------------------------

// The test components are pre-assembled WebAssembly component binaries, embedded as byte
// constants: the `wat` crate is not a dependency of this crate, and the fixtures must not
// depend on the assembly step at test time. Each was assembled once from WebAssembly text —
// the shape spot-bundle's own tests use: a host-function import canon-lowered against a
// tiny core module carrying `memory` and `realloc` — and validated with wasmparser at the
// same version and features this crate uses. Each exports its core module under the export
// name spot-pack's `wasm:` references ask for.

fn component(import: &str, export: &str, bytes: &[u8]) -> Vec<u8> {
    debug_assert!(!bytes.is_empty());
    let _ = (import, export);
    bytes.to_vec()
}

/// The component of the happy-path app: imports `log`, exports `on-save`.
pub fn plain_component() -> Vec<u8> {
    component("log", "on-save", PLAIN_COMPONENT)
}

/// A component reading documents (`get-doc` needs DocType read capability).
pub fn reading_component() -> Vec<u8> {
    component("get-doc", "on-read", READING_COMPONENT)
}

/// A component importing something the host does not provide.
pub fn stranger_component() -> Vec<u8> {
    component("make-coffee", "on-save", STRANGER_COMPONENT)
}

// --- the happy-path app -------------------------------------------------------------------------

/// One DocType definition that loads: the fixtures' single `customer` doctype, as plain TOML.
pub const CUSTOMER_DOCTYPE: &[u8] = b"name = \"customer\"\n\
     label = \"Customer\"\n\
     app = \"test_app\"\n\
     module = \"sales\"\n\
     \n\
     [[fields]]\n\
     name = \"customer_name\"\n\
     label = \"Customer Name\"\n\
     type = \"text\"\n\
     required = true\n";

/// One grant table that loads: a single role may read `customer`.
pub const GRANTS: &[u8] = b"[[entries]]\n\
     role = \"Reader\"\n\
     doctype = \"customer\"\n\
     rights = [\"read\"]\n";

/// The controllers of the happy path: the manifest's step (`test_app:on_save` — step names
/// are `<app>:<step>`, storage-style names) maps to the component's export.
pub const CONTROLLERS: &[u8] = b"[controllers]\n\"test_app:on_save\" = \"wasm:on-save\"\n";

/// The declared asset, so the manifest's `[[assets]]` row is satisfied.
pub const LOGO_SVG: &[u8] = b"<svg>\n</svg>\n";

/// A complete, valid app: manifest, doctype, permissions, controllers, component, asset.
/// Every broken-app fixture writes a variant of this and breaks one thing, so a failure
/// here means the harness broke, not the code under test. Returns the app directory.
#[must_use]
pub fn good_dir(root: &Path, extra: &[(&str, Vec<u8>)]) -> std::path::PathBuf {
    let dir = root.join("good-app");
    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("manifest.toml".to_owned(), manifest().into_bytes()),
        (
            "doctypes/customer.toml".to_owned(),
            CUSTOMER_DOCTYPE.to_vec(),
        ),
        ("permissions/grants.toml".to_owned(), GRANTS.to_vec()),
        ("controllers.toml".to_owned(), CONTROLLERS.to_vec()),
        ("assets/logo.svg".to_owned(), LOGO_SVG.to_vec()),
        ("app.wasm".to_owned(), plain_component()),
    ];
    for (rel, bytes) in extra {
        files.push(((*rel).to_owned(), bytes.clone()));
    }
    let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(r, b)| (r.as_str(), b.clone())).collect();
    write_app(&dir, &refs);
    dir
}

// --- the check-report assertions ----------------------------------------------------------------

/// `(code, path)` for every problem, so a test can assert the exact expected set with
/// `assert_eq!` on this list — codes AND paths both, in the report's stable order.
#[must_use]
pub fn have_codes(report: &CheckReport) -> Vec<(&'static str, Option<&str>)> {
    report
        .problems
        .iter()
        .map(|p| (p.code, p.path.as_deref()))
        .collect()
}

/// Panics naming the full report when `expected` does not match the problems exactly.
#[track_caller]
pub fn assert_problems(report: &CheckReport, expected: &[(&'static str, Option<&str>)]) {
    let got = have_codes(report);
    if got != expected {
        panic!(
            "expected problems {expected:?}, got {got:?}\n\
             full report:\n{}",
            spot_pack::report::check_text(report)
        );
    }
}

/// Asserts at least one problem sits at `path` and every problem there mentions `needle`, so
/// the test proves the message is about the fixture it wrote and not another file.
#[track_caller]
pub fn messages_about(report: &CheckReport, path: &str, needle: &str) {
    let found: Vec<&str> = report
        .problems
        .iter()
        .filter(|p| p.path.as_deref() == Some(path))
        .map(|p| p.message.as_str())
        .collect();
    assert!(
        !found.is_empty() && found.iter().all(|m| m.contains(needle)),
        "expected every problem at `{path}` to mention `{needle}`, got {found:?}\n\
         full report:\n{}",
        spot_pack::report::check_text(report)
    );
}

/// The panic message of a failed fixture expectation, as one string, for `assert!(.., ..)`.
#[track_caller]
pub fn codes_of(report: &CheckReport) -> String {
    format!(
        "{:?}\nfull report:\n{}",
        have_codes(report),
        spot_pack::report::check_text(report)
    )
}

// --- the signing material -----------------------------------------------------------------------

/// The fixture root of trust: a fixed seed, so every key id is stable across runs.
pub fn root() -> KeyPair {
    generate_keypair(&[1u8; 32])
}

/// The fixture publisher: the key the certificate names.
pub fn publisher() -> KeyPair {
    generate_keypair(&[2u8; 32])
}

/// A different publisher key, for the re-sign and revoke tests.
pub fn other_publisher() -> KeyPair {
    generate_keypair(&[3u8; 32])
}

/// The publisher certificate, valid over [100, 1200), issued by the test root.
pub fn certificate() -> SignedCertificate {
    certificate_window(100, 1200)
}

/// The publisher certificate over `[valid_from, valid_to)`, issued by the test root.
pub fn certificate_window(valid_from: i64, valid_to: i64) -> SignedCertificate {
    let body = PublisherCertificate {
        publisher_id: "acme".to_owned(),
        display_name: "Acme Software".to_owned(),
        key: publisher().public(),
        valid_from,
        valid_to,
        issued_by: root().public().key_id(),
        serial: 7,
    };
    issue_certificate(body, &root())
}

/// The certificate the `other` publisher controls, for the re-sign test.
pub fn certificate_for_other() -> SignedCertificate {
    let body = PublisherCertificate {
        publisher_id: "other".to_owned(),
        display_name: "Other Software".to_owned(),
        key: other_publisher().public(),
        valid_from: 100,
        valid_to: 1200,
        issued_by: root().public().key_id(),
        serial: 8,
    };
    issue_certificate(body, &root())
}

/// The trust anchor holding only the test root.
pub fn anchor() -> TrustAnchor {
    TrustAnchor::new(vec![TrustedRoot::new(root().public(), 0, None)])
}

/// An empty, correctly signed revocation list at `generation`.
pub fn empty_list(generation: u64) -> RevocationList {
    issue_revocation_list(
        RevocationBody {
            generation,
            issued_at: 0,
            issued_by: root().public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &root(),
    )
}

/// A list revoking `key` from `effective_from` on; `compromised` refuses signatures made
/// before `effective_from` too.
pub fn list_revoking(
    key: &KeyPair,
    effective_from: i64,
    compromised: bool,
    generation: u64,
) -> RevocationList {
    issue_revocation_list(
        RevocationBody {
            generation,
            issued_at: effective_from,
            issued_by: root().public().key_id(),
            keys: vec![RevokedKey {
                key_id: key.public().key_id(),
                effective_from,
                compromised,
                reason: "test revocation".to_owned(),
            }],
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &root(),
    )
}

/// Signs `bundle` for the fixture app identity at [`SIGNED_AT`] with `key`, which the
/// certificate must name. Uses [`spot_pack::KeyProvider`]'s implementation for `KeyPair`, so
/// no key file and no environment variable is involved.
pub fn signed_by(
    bundle: &spot_bundle::Bundle,
    key: &KeyPair,
    certificate: &SignedCertificate,
) -> BundleSignature {
    // A fresh pair over the same seed (KeyPair is not Clone by design), passed as the
    // KeyProvider it implements: no key file, no environment variable.
    let pair = spot_bundle_signing::generate_keypair(&key.to_seed_bytes());
    spot_pack::sign_packed(bundle, APP_ID, VERSION, certificate, &pair, SIGNED_AT)
        .expect("the key is the certificate's key")
}

/// A chain of one certificate.
pub fn chain(certificate: &SignedCertificate) -> CertificateChain {
    CertificateChain::new(certificate.clone())
}

/// A signature written as `pack --sign` writes it (JSON), for the CLI process tests.
pub fn signature_json(signature: &BundleSignature) -> Vec<u8> {
    format!("{}\n", signature.to_json()).into_bytes()
}

/// The certificate written as the CLI reads it (JSON).
pub fn certificate_json(certificate: &SignedCertificate) -> Vec<u8> {
    format!("{}\n", certificate.to_json()).into_bytes()
}

/// The trust-root file as the CLI reads it: one base64 public key per line.
pub fn trust_roots_text() -> Vec<u8> {
    use spot_bundle_signing::canonical::b64;
    format!("{}\n", b64(root().public().as_bytes())).into_bytes()
}

/// The revocation list file as the CLI reads it (JSON).
pub fn revocations_json(list: &RevocationList) -> Vec<u8> {
    format!("{}\n", list.to_json()).into_bytes()
}

/// The native names the fixtures' builds provide, for the `native:` reference tests.
pub fn native_names() -> BTreeSet<String> {
    BTreeSet::from(["on-save".to_owned()])
}
// === the pre-assembled component binaries ======================================================

pub const PLAIN_COMPONENT: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00, 0x07, 0x0e, 0x01, 0x40, 0x01, 0x07, 0x6d, 0x65,
    0x73, 0x73, 0x61, 0x67, 0x65, 0x73, 0x01, 0x00, 0x0a, 0x08, 0x01, 0x00, 0x03, 0x6c, 0x6f, 0x67,
    0x01, 0x00, 0x01, 0x91, 0x01, 0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x09, 0x01,
    0x60, 0x04, 0x7f, 0x7f, 0x7f, 0x7f, 0x01, 0x7f, 0x03, 0x02, 0x01, 0x00, 0x05, 0x03, 0x01, 0x00,
    0x01, 0x06, 0x07, 0x01, 0x7f, 0x01, 0x41, 0x80, 0x08, 0x0b, 0x07, 0x14, 0x02, 0x06, 0x6d, 0x65,
    0x6d, 0x6f, 0x72, 0x79, 0x02, 0x00, 0x07, 0x72, 0x65, 0x61, 0x6c, 0x6c, 0x6f, 0x63, 0x00, 0x00,
    0x0a, 0x13, 0x01, 0x11, 0x01, 0x01, 0x7f, 0x23, 0x00, 0x21, 0x04, 0x20, 0x04, 0x20, 0x03, 0x6a,
    0x24, 0x00, 0x20, 0x04, 0x0b, 0x00, 0x3f, 0x04, 0x6e, 0x61, 0x6d, 0x65, 0x00, 0x05, 0x04, 0x6c,
    0x69, 0x62, 0x63, 0x02, 0x28, 0x01, 0x00, 0x05, 0x00, 0x03, 0x6f, 0x6c, 0x64, 0x01, 0x08, 0x6f,
    0x6c, 0x64, 0x5f, 0x73, 0x69, 0x7a, 0x65, 0x02, 0x05, 0x61, 0x6c, 0x69, 0x67, 0x6e, 0x03, 0x08,
    0x6e, 0x65, 0x77, 0x5f, 0x73, 0x69, 0x7a, 0x65, 0x04, 0x03, 0x70, 0x74, 0x72, 0x07, 0x07, 0x01,
    0x00, 0x04, 0x68, 0x65, 0x61, 0x70, 0x02, 0x04, 0x01, 0x00, 0x00, 0x00, 0x06, 0x18, 0x02, 0x00,
    0x02, 0x01, 0x00, 0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x00, 0x00, 0x01, 0x00, 0x07, 0x72,
    0x65, 0x61, 0x6c, 0x6c, 0x6f, 0x63, 0x08, 0x0a, 0x01, 0x01, 0x00, 0x00, 0x03, 0x03, 0x00, 0x04,
    0x00, 0x00, 0x0b, 0x0e, 0x01, 0x00, 0x07, 0x6f, 0x6e, 0x2d, 0x73, 0x61, 0x76, 0x65, 0x00, 0x11,
    0x00, 0x00, 0x00, 0x33, 0x0e, 0x63, 0x6f, 0x6d, 0x70, 0x6f, 0x6e, 0x65, 0x6e, 0x74, 0x2d, 0x6e,
    0x61, 0x6d, 0x65, 0x01, 0x08, 0x00, 0x00, 0x01, 0x01, 0x03, 0x66, 0x5f, 0x6c, 0x01, 0x09, 0x00,
    0x11, 0x01, 0x00, 0x04, 0x6c, 0x69, 0x62, 0x63, 0x01, 0x06, 0x00, 0x12, 0x01, 0x00, 0x01, 0x63,
    0x01, 0x05, 0x01, 0x01, 0x00, 0x01, 0x66,
];
pub const READING_COMPONENT: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00, 0x07, 0x0e, 0x01, 0x40, 0x01, 0x07, 0x6d, 0x65,
    0x73, 0x73, 0x61, 0x67, 0x65, 0x73, 0x01, 0x00, 0x0a, 0x0c, 0x01, 0x00, 0x07, 0x67, 0x65, 0x74,
    0x2d, 0x64, 0x6f, 0x63, 0x01, 0x00, 0x01, 0x91, 0x01, 0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00,
    0x00, 0x01, 0x09, 0x01, 0x60, 0x04, 0x7f, 0x7f, 0x7f, 0x7f, 0x01, 0x7f, 0x03, 0x02, 0x01, 0x00,
    0x05, 0x03, 0x01, 0x00, 0x01, 0x06, 0x07, 0x01, 0x7f, 0x01, 0x41, 0x80, 0x08, 0x0b, 0x07, 0x14,
    0x02, 0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x02, 0x00, 0x07, 0x72, 0x65, 0x61, 0x6c, 0x6c,
    0x6f, 0x63, 0x00, 0x00, 0x0a, 0x13, 0x01, 0x11, 0x01, 0x01, 0x7f, 0x23, 0x00, 0x21, 0x04, 0x20,
    0x04, 0x20, 0x03, 0x6a, 0x24, 0x00, 0x20, 0x04, 0x0b, 0x00, 0x3f, 0x04, 0x6e, 0x61, 0x6d, 0x65,
    0x00, 0x05, 0x04, 0x6c, 0x69, 0x62, 0x63, 0x02, 0x28, 0x01, 0x00, 0x05, 0x00, 0x03, 0x6f, 0x6c,
    0x64, 0x01, 0x08, 0x6f, 0x6c, 0x64, 0x5f, 0x73, 0x69, 0x7a, 0x65, 0x02, 0x05, 0x61, 0x6c, 0x69,
    0x67, 0x6e, 0x03, 0x08, 0x6e, 0x65, 0x77, 0x5f, 0x73, 0x69, 0x7a, 0x65, 0x04, 0x03, 0x70, 0x74,
    0x72, 0x07, 0x07, 0x01, 0x00, 0x04, 0x68, 0x65, 0x61, 0x70, 0x02, 0x04, 0x01, 0x00, 0x00, 0x00,
    0x06, 0x18, 0x02, 0x00, 0x02, 0x01, 0x00, 0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x00, 0x00,
    0x01, 0x00, 0x07, 0x72, 0x65, 0x61, 0x6c, 0x6c, 0x6f, 0x63, 0x08, 0x0a, 0x01, 0x01, 0x00, 0x00,
    0x03, 0x03, 0x00, 0x04, 0x00, 0x00, 0x0b, 0x0e, 0x01, 0x00, 0x07, 0x6f, 0x6e, 0x2d, 0x72, 0x65,
    0x61, 0x64, 0x00, 0x11, 0x00, 0x00, 0x00, 0x33, 0x0e, 0x63, 0x6f, 0x6d, 0x70, 0x6f, 0x6e, 0x65,
    0x6e, 0x74, 0x2d, 0x6e, 0x61, 0x6d, 0x65, 0x01, 0x08, 0x00, 0x00, 0x01, 0x01, 0x03, 0x66, 0x5f,
    0x6c, 0x01, 0x09, 0x00, 0x11, 0x01, 0x00, 0x04, 0x6c, 0x69, 0x62, 0x63, 0x01, 0x06, 0x00, 0x12,
    0x01, 0x00, 0x01, 0x63, 0x01, 0x05, 0x01, 0x01, 0x00, 0x01, 0x66,
];
pub const STRANGER_COMPONENT: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00, 0x07, 0x0e, 0x01, 0x40, 0x01, 0x07, 0x6d, 0x65,
    0x73, 0x73, 0x61, 0x67, 0x65, 0x73, 0x01, 0x00, 0x0a, 0x10, 0x01, 0x00, 0x0b, 0x6d, 0x61, 0x6b,
    0x65, 0x2d, 0x63, 0x6f, 0x66, 0x66, 0x65, 0x65, 0x01, 0x00, 0x01, 0x91, 0x01, 0x00, 0x61, 0x73,
    0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x09, 0x01, 0x60, 0x04, 0x7f, 0x7f, 0x7f, 0x7f, 0x01, 0x7f,
    0x03, 0x02, 0x01, 0x00, 0x05, 0x03, 0x01, 0x00, 0x01, 0x06, 0x07, 0x01, 0x7f, 0x01, 0x41, 0x80,
    0x08, 0x0b, 0x07, 0x14, 0x02, 0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x02, 0x00, 0x07, 0x72,
    0x65, 0x61, 0x6c, 0x6c, 0x6f, 0x63, 0x00, 0x00, 0x0a, 0x13, 0x01, 0x11, 0x01, 0x01, 0x7f, 0x23,
    0x00, 0x21, 0x04, 0x20, 0x04, 0x20, 0x03, 0x6a, 0x24, 0x00, 0x20, 0x04, 0x0b, 0x00, 0x3f, 0x04,
    0x6e, 0x61, 0x6d, 0x65, 0x00, 0x05, 0x04, 0x6c, 0x69, 0x62, 0x63, 0x02, 0x28, 0x01, 0x00, 0x05,
    0x00, 0x03, 0x6f, 0x6c, 0x64, 0x01, 0x08, 0x6f, 0x6c, 0x64, 0x5f, 0x73, 0x69, 0x7a, 0x65, 0x02,
    0x05, 0x61, 0x6c, 0x69, 0x67, 0x6e, 0x03, 0x08, 0x6e, 0x65, 0x77, 0x5f, 0x73, 0x69, 0x7a, 0x65,
    0x04, 0x03, 0x70, 0x74, 0x72, 0x07, 0x07, 0x01, 0x00, 0x04, 0x68, 0x65, 0x61, 0x70, 0x02, 0x04,
    0x01, 0x00, 0x00, 0x00, 0x06, 0x18, 0x02, 0x00, 0x02, 0x01, 0x00, 0x06, 0x6d, 0x65, 0x6d, 0x6f,
    0x72, 0x79, 0x00, 0x00, 0x01, 0x00, 0x07, 0x72, 0x65, 0x61, 0x6c, 0x6c, 0x6f, 0x63, 0x08, 0x0a,
    0x01, 0x01, 0x00, 0x00, 0x03, 0x03, 0x00, 0x04, 0x00, 0x00, 0x0b, 0x0e, 0x01, 0x00, 0x07, 0x6f,
    0x6e, 0x2d, 0x73, 0x61, 0x76, 0x65, 0x00, 0x11, 0x00, 0x00, 0x00, 0x33, 0x0e, 0x63, 0x6f, 0x6d,
    0x70, 0x6f, 0x6e, 0x65, 0x6e, 0x74, 0x2d, 0x6e, 0x61, 0x6d, 0x65, 0x01, 0x08, 0x00, 0x00, 0x01,
    0x01, 0x03, 0x66, 0x5f, 0x6c, 0x01, 0x09, 0x00, 0x11, 0x01, 0x00, 0x04, 0x6c, 0x69, 0x62, 0x63,
    0x01, 0x06, 0x00, 0x12, 0x01, 0x00, 0x01, 0x63, 0x01, 0x05, 0x01, 0x01, 0x00, 0x01, 0x66,
];
