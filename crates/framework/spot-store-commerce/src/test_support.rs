//! Test-only fixtures shared by the crate's unit tests.
//!
//! Nothing here is compiled into the product: the module is behind `#[cfg(test)]`. The
//! `TestSigner` holds a throwaway Ed25519 key drawn from a fixed seed so a signature is a
//! real signature; no key material of any real store ever touches the tests.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use spot_bundle_signing::{BundleSignature, CertificateChain, KeyId, PublicKey};
use spot_bundle_signing::{PublisherCertificate, SignedCertificate, generate_keypair};
use spot_money::{Currency, CurrencyTable};
use spot_registry::{AppName, Digest, Release, SignatureRef, Version};

use crate::licence::StoreSigner;

/// A throwaway Ed25519 signer for tests: a real key pair from a fixed seed.
pub struct TestSigner {
    signing: ed25519_dalek::SigningKey,
    key_id: String,
}

impl TestSigner {
    /// A signer whose seed is `seed_byte` repeated (with the first byte set so the seed is
    /// not all-equal).
    pub fn new(seed_byte: u8) -> Self {
        let mut seed = [seed_byte; 32];
        seed[0] = 1;
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();
        let key_id = KeyId::of(&PublicKey::from_bytes(&public).unwrap());
        Self {
            signing,
            key_id: key_id.as_str().to_owned(),
        }
    }

    /// The public key, validated as `spot-bundle-signing` sees it.
    pub fn public(&self) -> PublicKey {
        PublicKey::from_bytes(&self.public_key_bytes()).unwrap()
    }
}

impl StoreSigner for TestSigner {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn public_key_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer as _;
        self.signing.sign(message).to_bytes()
    }
}

/// The test currency table: USD with two minor-unit digits.
pub fn table() -> CurrencyTable {
    CurrencyTable::new([(usd(), 2)]).unwrap()
}

/// The test currency, USD.
pub fn usd() -> Currency {
    Currency::new("USD").unwrap()
}

/// A registry release fixture: a signed, active release for `app` `version`.
pub fn release(app: &str, publisher: &str, version: &str) -> Release {
    let digest: Digest = "ab".repeat(32).parse().unwrap();
    let key_id = KeyId::parse("0123456789abcdef0123456789abcdef").unwrap();
    let certificate = PublisherCertificate {
        publisher_id: publisher.to_owned(),
        display_name: publisher.to_owned(),
        key: generate_keypair(&[7u8; 32]).public(),
        valid_from: 0,
        valid_to: 4_000_000_000,
        issued_by: key_id.clone(),
        serial: 1,
    };
    let chain = CertificateChain::new(SignedCertificate::from_parts(certificate, [0u8; 64]));
    let signature = BundleSignature::from_parts(
        app.to_owned(),
        version.to_owned(),
        publisher.to_owned(),
        key_id,
        1_000,
        [0u8; 64],
    );
    Release {
        app: AppName::parse(app).unwrap(),
        version: version.parse::<Version>().unwrap(),
        digest,
        size: 10,
        requires_host: None,
        depends: Vec::new(),
        published_at: 1_000,
        publisher: publisher.to_owned(),
        yanked: None,
        withdrawn: None,
        signature_ref: SignatureRef { signature, chain },
        title: "Test App".to_owned(),
        description: String::new(),
        category: String::new(),
        capabilities: Default::default(),
    }
}
