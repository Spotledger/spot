//! `verify` and the parsers never panic on arbitrary bytes. A seeded generator makes the cases
//! reproducible without a property-testing dependency: random bytes, and valid documents with
//! random bytes changed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bundle_signing::*;

/// A xorshift64 generator with a fixed seed, so every run sees the same cases.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = usize::try_from(self.next() % (max_len as u64 + 1)).unwrap_or(0);
        (0..len)
            .map(|_| u8::try_from(self.next() & 0xff).unwrap_or(0))
            .collect()
    }
}

/// Parses `bytes` as each document, and verifies whatever parses, under the fixture's trust.
fn exercise(bytes: &[u8]) {
    let bundle = good();
    let digest = bundle.digest();
    let facts = facts(&digest, VERSION);
    let sig = BundleSignature::from_json(bytes);
    let cert = SignedCertificate::from_json(bytes);
    let list = RevocationList::from_json(bytes);
    if let (Ok(sig), Ok(cert), Ok(list)) = (&sig, &cert, &list) {
        let _ = verify(
            &facts,
            sig,
            &CertificateChain::new(cert.clone()),
            &anchor(),
            list,
            NOW,
            &install(),
        );
    }
    if let (Ok(sig), Ok(list)) = (&sig, &list) {
        let _ = verify(
            &facts,
            sig,
            &chain(certificate()),
            &anchor(),
            list,
            NOW,
            &install(),
        );
    }
    if let Ok(cert) = &cert {
        let _ = verify(
            &facts,
            &sign(&bundle, APP_ID, VERSION, SIGNED_AT),
            &CertificateChain::new(cert.clone()),
            &anchor(),
            &empty_list(1),
            NOW,
            &install(),
        );
    }
}

#[test]
fn arbitrary_bytes_never_panic() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..800 {
        exercise(&rng.bytes(400));
    }
}

#[test]
fn valid_documents_with_changed_bytes_never_panic() {
    let bundle = good();
    let sig_json = sign(&bundle, APP_ID, VERSION, SIGNED_AT)
        .to_json()
        .into_bytes();
    let cert_json = certificate().to_json().into_bytes();
    let list_json = empty_list(1).to_json().into_bytes();
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for original in [sig_json, cert_json, list_json] {
        for _ in 0..400 {
            let mut mutated = original.clone();
            let index = usize::try_from(rng.next() % mutated.len() as u64).unwrap_or(0);
            mutated[index] = u8::try_from(rng.next() & 0xff).unwrap_or(0);
            exercise(&mutated);
        }
    }
}

#[test]
fn the_valid_documents_round_trip() {
    let bundle = good();
    let sig = sign(&bundle, APP_ID, VERSION, SIGNED_AT);
    assert_eq!(
        BundleSignature::from_json(sig.to_json().as_bytes()).unwrap(),
        sig
    );
    let cert = certificate();
    assert_eq!(
        SignedCertificate::from_json(cert.to_json().as_bytes()).unwrap(),
        cert
    );
    let list = empty_list(1);
    assert_eq!(
        RevocationList::from_json(list.to_json().as_bytes()).unwrap(),
        list
    );
}

#[test]
fn an_unknown_field_or_format_is_refused() {
    let sig = sign(&good(), APP_ID, VERSION, SIGNED_AT).to_json();
    let with_extra = sig.replacen('{', "{\n  \"extra\": 1,", 1);
    assert_eq!(
        BundleSignature::from_json(with_extra.as_bytes())
            .unwrap_err()
            .code(),
        "signing.document_malformed"
    );
    let wrong_format = sig.replacen("spot-bundle-signature", "spot-something-else", 1);
    assert_eq!(
        BundleSignature::from_json(wrong_format.as_bytes())
            .unwrap_err()
            .code(),
        "signing.document_malformed"
    );
}
