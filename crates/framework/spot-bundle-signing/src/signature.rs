//! Bundle signatures: a detached Ed25519 signature over the bundle's canonical digest, bound to
//! the app id, the version and the publisher, so it cannot be moved to another app or version.
//!
//! The signed bytes are `BUNDLE_DOMAIN || canonical{app_id, app_version, digest, key_id,
//! publisher_id, signed_at}`. The digest is the one [`spot_bundle::Digest`] gives, so an
//! archive repacked with the same content verifies, and one changed byte does not.

use std::collections::BTreeMap;

use spot_bundle::Digest;

use crate::canonical::{self, BUNDLE_DOMAIN};
use crate::certificate::SignedCertificate;
use crate::error::SigningError;
use crate::keys::{KeyId, KeyPair, SIGNATURE_BYTES};

/// A bundle's signature and the identity it was made for. It ships next to the bundle as a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSignature {
    /// The app's id, as the manifest names it.
    pub app_id: String,
    /// The app's version, as the manifest names it.
    pub app_version: String,
    /// The publisher the signing certificate names.
    pub publisher_id: String,
    /// The signing key's id.
    pub key_id: KeyId,
    /// When the publisher signed (UTC seconds).
    pub signed_at: i64,
    pub(crate) signature: [u8; SIGNATURE_BYTES],
}

impl BundleSignature {
    /// The signature bytes.
    #[must_use]
    pub fn signature(&self) -> &[u8; SIGNATURE_BYTES] {
        &self.signature
    }

    /// Assembles a signature from its parts without checking it. Only verification checks it.
    #[must_use]
    pub fn from_parts(
        app_id: String,
        app_version: String,
        publisher_id: String,
        key_id: KeyId,
        signed_at: i64,
        signature: [u8; SIGNATURE_BYTES],
    ) -> Self {
        Self {
            app_id,
            app_version,
            publisher_id,
            key_id,
            signed_at,
            signature,
        }
    }
}

/// The bytes a bundle signature covers, for a digest and the identity fields of a signature.
#[must_use]
pub(crate) fn signed_bytes(
    digest: &Digest,
    app_id: &str,
    app_version: &str,
    publisher_id: &str,
    key_id: &KeyId,
    signed_at: i64,
) -> Vec<u8> {
    let mut fields = BTreeMap::new();
    fields.insert("app_id", app_id.into());
    fields.insert("app_version", app_version.into());
    fields.insert("digest", digest.to_hex().into());
    fields.insert("key_id", key_id.as_str().into());
    fields.insert("publisher_id", publisher_id.into());
    fields.insert("signed_at", signed_at.into());
    canonical::encode(BUNDLE_DOMAIN, &fields)
}

/// Signs a bundle for `app_id` at `app_version`, as the publisher named by `certificate`, with
/// the key pair the certificate names.
///
/// # Errors
/// [`SigningError::KeyMismatch`] when `key` is not the certificate's key.
pub fn sign_bundle(
    digest: &Digest,
    app_id: &str,
    app_version: &str,
    certificate: &SignedCertificate,
    key: &KeyPair,
    signed_at: i64,
) -> Result<BundleSignature, SigningError> {
    if key.public() != certificate.body.key {
        return Err(SigningError::KeyMismatch);
    }
    let key_id = key.public().key_id();
    let message = signed_bytes(
        digest,
        app_id,
        app_version,
        &certificate.body.publisher_id,
        &key_id,
        signed_at,
    );
    Ok(BundleSignature {
        app_id: app_id.to_owned(),
        app_version: app_version.to_owned(),
        publisher_id: certificate.body.publisher_id.clone(),
        key_id,
        signed_at,
        signature: key.sign(&message),
    })
}
