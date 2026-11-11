//! Signing and verifying a packed bundle.
//!
//! The signing key is supplied by a [`KeyProvider`] — a file path (the wire form is the 32
//! seed bytes, standard base64) or a caller-injected implementation. Key material never
//! arrives through an environment variable and is never printed: a provider's `Debug` shows
//! the file, never the bytes, and key errors name the file, never the value.

use std::fmt;
use std::path::{Path, PathBuf};

use spot_bundle::Bundle;
use spot_bundle_signing::{
    BundleFacts, BundleSignature, CertificateChain, KeyId, KeyPair, RevocationList, TrustAnchor,
    Verified, VerifyMode, VerifyPolicy, generate_keypair, sign_bundle, verify,
};

use crate::error::PackError;

/// Supplies the publisher's signing key pair. Inject this, or use [`FileKeyProvider`];
/// never wire key material through an environment variable.
pub trait KeyProvider: fmt::Debug {
    /// The key pair this provider holds.
    ///
    /// # Errors
    /// [`PackError::KeyUnreadable`] when the provider cannot produce a usable key.
    fn key_pair(&self) -> Result<KeyPair, PackError>;
}

impl KeyProvider for KeyPair {
    fn key_pair(&self) -> Result<KeyPair, PackError> {
        // A fresh pair over the same seed: `KeyPair` is not `Clone` by design.
        Ok(generate_keypair(&self.to_seed_bytes()))
    }
}

/// Reads the publisher's key from a file: exactly 32 seed bytes, standard base64, with
/// surrounding whitespace allowed. A private key lives in the author's key store; the
/// reader never echoes what it read.
#[derive(Clone)]
pub struct FileKeyProvider {
    path: PathBuf,
}

impl FileKeyProvider {
    /// A provider for the key file at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file this provider reads.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for FileKeyProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The path, never the bytes: a `Debug` of a provider must not print key material.
        write!(f, "FileKeyProvider({})", self.path.display())
    }
}

impl KeyProvider for FileKeyProvider {
    fn key_pair(&self) -> Result<KeyPair, PackError> {
        let text = std::fs::read_to_string(&self.path).map_err(|_| PackError::KeyUnreadable)?;
        let seed = decode_seed(text.trim()).ok_or(PackError::KeyUnreadable)?;
        Ok(generate_keypair(&seed))
    }
}

/// Decodes one base64 key file into 32 seed bytes.
fn decode_seed(text: &str) -> Option<[u8; 32]> {
    let bytes = spot_bundle_signing::canonical::unb64(text)?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

/// Writes a signing key's seed bytes to a file, in the form [`FileKeyProvider`] reads.
///
/// For the author's own key store. The bytes never pass through a log or a `Debug`.
///
/// # Errors
/// [`PackError::KeyUnreadable`] when the file cannot be written.
pub fn write_key_file(key: &KeyPair, path: &Path) -> Result<(), PackError> {
    use spot_bundle_signing::canonical::b64;
    std::fs::write(path, b64(&key.to_seed_bytes()) + "\n").map_err(|_| PackError::KeyUnreadable)
}

/// Signs a packed bundle.
///
/// `app_id` and `version` are the manifest's identity; `certificate` names the publisher;
/// `signed_at` is the caller's instant (this crate reads no clock).
///
/// # Errors
/// [`PackError::KeyMismatch`] when the provider's key is not the certificate's key, and
/// [`PackError::KeyUnreadable`] when the provider fails.
pub fn sign_packed(
    bundle: &Bundle,
    app_id: &str,
    version: &str,
    certificate: &spot_bundle_signing::SignedCertificate,
    provider: &dyn KeyProvider,
    signed_at: i64,
) -> Result<BundleSignature, PackError> {
    let key = provider.key_pair()?;
    sign_bundle(
        &bundle.digest(),
        app_id,
        version,
        certificate,
        &key,
        signed_at,
    )
    .map_err(|_| PackError::KeyMismatch)
}

/// What a successful [`verify_packed`] establishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleProof {
    /// The publisher the certificate names.
    pub publisher_id: String,
    /// The signing key.
    pub key_id: KeyId,
    /// When the bundle was signed (UTC seconds).
    pub signed_at: i64,
    /// The revocation list generation the check used.
    pub revocation_generation: u64,
    /// The bundle's canonical digest, the value the signature covered.
    pub digest: spot_bundle::Digest,
}

/// Verifies a bundle's signature the way the installer does, with the caller's already-loaded
/// signature file, certificate, trust anchor and revocation list, and the caller's `now`.
///
/// # Errors
/// [`crate::error::VerifyRefusal`] — the signing layer's typed refusal, carrying its
/// `signing.*` code — when any check fails.
#[allow(clippy::too_many_arguments)]
pub fn verify_packed(
    bundle: &Bundle,
    signature: &BundleSignature,
    chain: &CertificateChain,
    trust_roots: &TrustAnchor,
    revocations: &RevocationList,
    now: i64,
    policy: &VerifyPolicy,
) -> Result<BundleProof, crate::error::VerifyRefusal> {
    let digest = bundle.digest();
    let manifest = bundle.manifest();
    let facts = BundleFacts {
        digest: &digest,
        app_id: manifest.app.name.as_str(),
        version: &manifest.app.version.to_string(),
    };
    let verified: Verified = verify(
        &facts,
        signature,
        chain,
        trust_roots,
        revocations,
        now,
        policy,
    )
    .map_err(crate::error::VerifyRefusal)?;
    Ok(BundleProof {
        publisher_id: verified.publisher_id,
        key_id: verified.key_id,
        signed_at: verified.signed_at,
        revocation_generation: verified.revocation_generation,
        digest,
    })
}

/// The install-time policy: refused expired certificates, no revocation generation held.
#[must_use]
pub fn install_policy() -> VerifyPolicy {
    VerifyPolicy {
        mode: VerifyMode::Install,
        allow_expired_certificate_at_startup: false,
        held_revocation_generation: 0,
    }
}
