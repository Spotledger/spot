//! The pinned IdP signing keys: read a public key out of an X.509 certificate and verify a
//! signature with it.
//!
//! A SAML message may carry its own `ds:KeyInfo`/`X509Certificate`; that certificate is never
//! used to choose a key. The only keys this crate verifies with are the certificates the tenant
//! administrator pinned in configuration, and a certificate *in the message* is only ever
//! compared against that pinned set (see [`matches_pinned`]). This is what stops the "attacker
//! certificate in KeyInfo" substitution.

use x509_parser::parse_x509_certificate;
use x509_parser::public_key::PublicKey as ParsedPublicKey;

use crate::oidc::jwks::PublicKey;
use crate::provider::Jwa;

/// The public key extracted from a certificate, in the form `ring` verifies with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CertificateKey {
    /// An RSA key (modulus and exponent).
    Rsa(PublicKey),
    /// A P-256 key.
    Ec(PublicKey),
}

/// Extracts the public key from DER-encoded X.509 certificate bytes.
///
/// Returns `None` for a certificate that does not parse or that carries a key type this crate
/// does not verify with (only RSA and P-256 are accepted; the rest are ignored, never trusted).
#[must_use]
pub(crate) fn certificate_public_key(der: &[u8]) -> Option<CertificateKey> {
    let (_, certificate) = parse_x509_certificate(der).ok()?;
    match certificate.public_key().parsed().ok()? {
        ParsedPublicKey::RSA(rsa) => Some(CertificateKey::Rsa(PublicKey::Rsa {
            n: strip_leading_zeros(rsa.modulus).to_vec(),
            e: strip_leading_zeros(rsa.exponent).to_vec(),
        })),
        ParsedPublicKey::EC(point) => {
            let data = point.data();
            if data.len() != 65 || data[0] != 0x04 {
                return None;
            }
            let mut x = [0u8; 32];
            let mut y = [0u8; 32];
            x.copy_from_slice(&data[1..33]);
            y.copy_from_slice(&data[33..65]);
            Some(CertificateKey::Ec(PublicKey::Ec { x, y }))
        }
        _ => None,
    }
}

/// Verifies `signature` over `message` with every pinned certificate in turn.
///
/// The algorithm must match the key type, so an RSA signature is never verified with an EC key
/// and vice versa. Returns `true` when one certificate verifies.
#[must_use]
pub(crate) fn verify_with_certificates(
    certificates: &[Vec<u8>],
    alg: Jwa,
    message: &[u8],
    signature: &[u8],
) -> bool {
    certificates.iter().any(|der| {
        certificate_public_key(der)
            .is_some_and(|key| key.public_key().verify(alg, message, signature))
    })
}

/// Whether a certificate carried in a message equals one of the pinned certificates.
#[must_use]
pub(crate) fn matches_pinned(certificate: &[u8], pinned: &[Vec<u8>]) -> bool {
    pinned.iter().any(|known| known.as_slice() == certificate)
}

impl CertificateKey {
    /// The inner public key.
    fn public_key(&self) -> &PublicKey {
        match self {
            Self::Rsa(key) | Self::Ec(key) => key,
        }
    }
}

/// Removes DER INTEGER sign-padding zeros (a leading `0x00` when the top bit is set).
fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let mut index = 0;
    while index + 1 < bytes.len() && bytes[index] == 0 {
        index += 1;
    }
    &bytes[index..]
}
