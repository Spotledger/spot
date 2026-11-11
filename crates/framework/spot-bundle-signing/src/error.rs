//! Typed errors of the signing layer, with stable `signing.*` codes.
//!
//! Verification fails on the first check that does not hold, and that check's code names the
//! refusal. Callers match on the code, never on the text.

/// Every error this crate returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SigningError {
    /// A public key is not 32 bytes or not a valid Ed25519 point.
    #[error("a public key is malformed")]
    KeyMalformed,
    /// A signature is not 64 bytes, or a key id is not 32 lower-case hex characters.
    #[error("a signature or key id is malformed")]
    SignatureMalformed,
    /// A JSON document (certificate, signature file, revocation list) does not parse as its format.
    #[error("the {document} document is malformed: {detail}")]
    DocumentMalformed {
        /// Which document.
        document: &'static str,
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The signing key given to `sign_bundle` is not the certificate's key.
    #[error("the signing key is not the certificate's key")]
    KeyMismatch,
    /// The certificate names an issuer that is not a trusted root.
    #[error("the certificate is issued by a root this store does not trust")]
    UnknownRoot,
    /// The issuing root's validity window does not cover the check time.
    #[error("the issuing root is not valid at this time")]
    RootOutsideWindow,
    /// The certificate's signature does not verify under the root it names.
    #[error("the certificate's signature does not verify under its issuer")]
    CertificateSignatureInvalid,
    /// The certificate is not yet valid.
    #[error("the publisher certificate is not yet valid")]
    CertificateNotYetValid,
    /// The certificate has expired.
    #[error("the publisher certificate has expired")]
    CertificateExpired,
    /// The signature claims a signing time outside the certificate's validity window.
    #[error("the signature's signing time is outside the certificate's validity window")]
    SignedOutsideCertificateWindow,
    /// The signature names a publisher other than the certificate's.
    #[error("the signature names another publisher than its certificate")]
    PublisherMismatch,
    /// The publisher was revoked at or before the signature's time.
    #[error("the publisher was revoked before this bundle was signed")]
    PublisherRevoked,
    /// The signing key was revoked at or before the signature's time.
    #[error("the signing key was revoked before this bundle was signed")]
    KeyRevoked,
    /// The signing key is revoked as compromised; every signature by it is refused.
    #[error("the signing key is revoked as compromised")]
    KeyCompromised,
    /// The signature names a key other than the certificate's.
    #[error("the signature names another key than its certificate")]
    SignatureKeyMismatch,
    /// This exact bundle digest is on the revocation list.
    #[error("this bundle has been revoked")]
    BundleRevoked,
    /// The Ed25519 signature does not verify over the bundle's digest, app id and version.
    #[error("the signature does not verify over this bundle")]
    SignatureInvalid,
    /// The signature was made for another app than the manifest names.
    #[error("the signature was made for another app")]
    AppMismatch,
    /// The signature was made for another version than the manifest names.
    #[error("the signature was made for another version")]
    VersionMismatch,
    /// The revocation list's signature does not verify under its issuer.
    #[error("the revocation list's signature does not verify under its issuer")]
    RevocationSignatureInvalid,
    /// The revocation list is older than the one the caller already holds.
    #[error("the revocation list is older than the one already held")]
    StaleRevocationList,
}

impl SigningError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyMalformed => "signing.key_malformed",
            Self::SignatureMalformed => "signing.signature_malformed",
            Self::DocumentMalformed { .. } => "signing.document_malformed",
            Self::KeyMismatch => "signing.key_mismatch",
            Self::UnknownRoot => "signing.unknown_root",
            Self::RootOutsideWindow => "signing.root_outside_window",
            Self::CertificateSignatureInvalid => "signing.certificate_signature_invalid",
            Self::CertificateNotYetValid => "signing.certificate_not_yet_valid",
            Self::CertificateExpired => "signing.certificate_expired",
            Self::SignedOutsideCertificateWindow => "signing.signed_outside_certificate_window",
            Self::PublisherMismatch => "signing.publisher_mismatch",
            Self::PublisherRevoked => "signing.publisher_revoked",
            Self::KeyRevoked => "signing.key_revoked",
            Self::KeyCompromised => "signing.key_compromised",
            Self::SignatureKeyMismatch => "signing.signature_key_mismatch",
            Self::BundleRevoked => "signing.bundle_revoked",
            Self::SignatureInvalid => "signing.signature_invalid",
            Self::AppMismatch => "signing.app_mismatch",
            Self::VersionMismatch => "signing.version_mismatch",
            Self::RevocationSignatureInvalid => "signing.revocation_signature_invalid",
            Self::StaleRevocationList => "signing.stale_revocation_list",
        }
    }
}
