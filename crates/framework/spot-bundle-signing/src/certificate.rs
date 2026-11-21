//! Publisher certificates: a publisher's Ed25519 key, named and dated, signed by a store root.

use std::collections::BTreeMap;

use crate::canonical::{self, CERTIFICATE_DOMAIN};
use crate::keys::{KeyId, KeyPair, PublicKey, SIGNATURE_BYTES};

/// The body of a publisher certificate. Its signed bytes are [`PublisherCertificate::signed_bytes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherCertificate {
    /// The publisher's stable id.
    pub publisher_id: String,
    /// The name shown to an admin.
    pub display_name: String,
    /// The publisher's signing key.
    pub key: PublicKey,
    /// First instant (UTC seconds) the certificate is valid.
    pub valid_from: i64,
    /// First instant the certificate is no longer valid.
    pub valid_to: i64,
    /// The root that issued it.
    pub issued_by: KeyId,
    /// The issuer's serial number for this certificate.
    pub serial: u64,
}

impl PublisherCertificate {
    /// The domain-separated canonical bytes the root signs.
    #[must_use]
    pub fn signed_bytes(&self) -> Vec<u8> {
        let mut fields = BTreeMap::new();
        fields.insert("display_name", self.display_name.as_str().into());
        fields.insert("issued_by", self.issued_by.as_str().into());
        fields.insert("publisher_id", self.publisher_id.as_str().into());
        fields.insert("public_key", canonical::b64(self.key.as_bytes()).into());
        fields.insert("serial", self.serial.into());
        fields.insert("valid_from", self.valid_from.into());
        fields.insert("valid_to", self.valid_to.into());
        canonical::encode(CERTIFICATE_DOMAIN, &fields)
    }
}

/// A certificate with the issuing root's signature over its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedCertificate {
    /// The certificate body.
    pub body: PublisherCertificate,
    pub(crate) signature: [u8; SIGNATURE_BYTES],
}

impl SignedCertificate {
    /// The signature bytes.
    #[must_use]
    pub fn signature(&self) -> &[u8; SIGNATURE_BYTES] {
        &self.signature
    }

    /// Assembles a signed certificate from its parts without checking the signature. Only
    /// verification checks it.
    #[must_use]
    pub fn from_parts(body: PublisherCertificate, signature: [u8; SIGNATURE_BYTES]) -> Self {
        Self { body, signature }
    }
}

/// Issues a certificate: `root` signs the body. The caller names the root in `body.issued_by`;
/// [`crate::verify`] refuses the certificate if the signing key is not that root.
#[must_use]
pub fn issue_certificate(body: PublisherCertificate, root: &KeyPair) -> SignedCertificate {
    let signature = root.sign(&body.signed_bytes());
    SignedCertificate { body, signature }
}

/// The certificates a publisher presents. Version 1 has no intermediates: the leaf is issued
/// directly by a store root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateChain {
    /// The publisher's certificate.
    pub leaf: SignedCertificate,
}

impl CertificateChain {
    /// A chain of one certificate.
    #[must_use]
    pub fn new(leaf: SignedCertificate) -> Self {
        Self { leaf }
    }
}
