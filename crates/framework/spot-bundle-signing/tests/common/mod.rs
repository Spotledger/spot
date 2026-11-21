//! Fixtures for the signing tests: two real bundles that differ by one byte (built with
//! `spot-bundle`), a publisher and a root with fixed seeds, and the trust material.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_bundle::{Bundle, BundleBuilder, HostInfo, Limits};
use spot_bundle_signing::{
    BundleFacts, BundleSignature, CertificateChain, KeyPair, PublisherCertificate, RevocationBody,
    RevocationList, SignedCertificate, TrustAnchor, TrustedRoot, VerifyMode, VerifyPolicy,
    generate_keypair, issue_certificate, issue_revocation_list, sign_bundle,
};

pub const APP_ID: &str = "test_app";
pub const VERSION: &str = "1.0.0";
pub const SIGNED_AT: i64 = 150;
pub const NOW: i64 = 200;

/// The host version the bundles open for.
pub fn host() -> HostInfo {
    HostInfo {
        version: semver::Version::new(0, 1, 0),
    }
}

/// The manifest, with a comment whose last letter is `tag`; one byte of it changes the digest.
pub fn manifest(tag: char, version: &str) -> String {
    format!("[app]\nname = \"{APP_ID}\"\nversion = \"{version}\"\n# build {tag}\n")
}

/// An empty component with a custom section holding `payload`: a valid binary whose one payload
/// byte can be changed.
fn component_with(payload: &[u8]) -> Vec<u8> {
    // The component header: magic, the component-model version 0x0d, layer 1. An empty component.
    let mut bytes = vec![0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];
    let mut content = vec![3u8];
    content.extend_from_slice(b"tag");
    content.extend_from_slice(payload);
    bytes.push(0);
    bytes.push(u8::try_from(content.len()).expect("a short section"));
    bytes.extend_from_slice(&content);
    bytes
}

/// A bundle of `manifest` and a component carrying `payload`, opened as the host would.
pub fn open(manifest: &str, payload: &[u8]) -> Bundle {
    let bytes = BundleBuilder::new(manifest.to_owned(), component_with(payload))
        .build()
        .unwrap_or_else(|e| panic!("the bundle builds: {e:?}"));
    Bundle::open(&bytes, &Limits::new(), &host())
        .unwrap_or_else(|e| panic!("the bundle opens: {e:?}"))
}

/// The good bundle: version 1.0.0, payload `a`.
pub fn good() -> Bundle {
    open(&manifest('a', VERSION), b"a")
}

pub fn root() -> KeyPair {
    generate_keypair(&[1u8; 32])
}

pub fn publisher() -> KeyPair {
    generate_keypair(&[2u8; 32])
}

pub fn other_key() -> KeyPair {
    generate_keypair(&[3u8; 32])
}

/// The publisher's certificate, valid over [100, 1000), issued by the root named by `issuer`.
pub fn certificate_under(issuer: &KeyPair, valid_to: i64) -> SignedCertificate {
    let body = PublisherCertificate {
        publisher_id: "acme".to_owned(),
        display_name: "Acme Software".to_owned(),
        key: publisher().public(),
        valid_from: 100,
        valid_to,
        issued_by: issuer.public().key_id(),
        serial: 7,
    };
    issue_certificate(body, issuer)
}

pub fn certificate() -> SignedCertificate {
    certificate_under(&root(), 1000)
}

pub fn chain(certificate: SignedCertificate) -> CertificateChain {
    CertificateChain::new(certificate)
}

/// The store trusts one root, valid from the start of time.
pub fn anchor() -> TrustAnchor {
    TrustAnchor::new(vec![TrustedRoot::new(root().public(), 0, None)])
}

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

/// A signature by the publisher over `bundle` for `app` at `version`.
pub fn sign(bundle: &Bundle, app: &str, version: &str, signed_at: i64) -> BundleSignature {
    sign_bundle(
        &bundle.digest(),
        app,
        version,
        &certificate(),
        &publisher(),
        signed_at,
    )
    .expect("the key is the certificate's key")
}

pub fn install() -> VerifyPolicy {
    VerifyPolicy {
        mode: VerifyMode::Install,
        allow_expired_certificate_at_startup: false,
        held_revocation_generation: 0,
    }
}

/// The facts of `bundle` as the host holds them: its digest and its manifest identity.
pub fn facts<'a>(digest: &'a spot_bundle::Digest, version: &'a str) -> BundleFacts<'a> {
    BundleFacts {
        digest,
        app_id: APP_ID,
        version,
    }
}

/// The error code of a refusal, or panics when the check passed.
pub fn refused_with<T: std::fmt::Debug>(
    result: Result<T, spot_bundle_signing::SigningError>,
) -> &'static str {
    match result {
        Ok(value) => panic!("expected a refusal, the check passed: {value:?}"),
        Err(error) => error.code(),
    }
}

/// The publisher's certificate over `[valid_from, valid_to)`, issued by the test root.
pub fn certificate_window(valid_from: i64, valid_to: i64) -> SignedCertificate {
    let body = PublisherCertificate {
        publisher_id: "acme".to_owned(),
        display_name: "Acme Software".to_owned(),
        key: publisher().public(),
        valid_from,
        valid_to,
        issued_by: root().public().key_id(),
        serial: 8,
    };
    issue_certificate(body, &root())
}

/// A signature by the publisher over `bundle` claiming `signed_at`, whatever the window says.
pub fn sign_at(
    bundle: &Bundle,
    signed_at: i64,
    certificate: &SignedCertificate,
) -> BundleSignature {
    sign_bundle(
        &bundle.digest(),
        APP_ID,
        VERSION,
        certificate,
        &publisher(),
        signed_at,
    )
    .expect("the key is the certificate's key")
}
