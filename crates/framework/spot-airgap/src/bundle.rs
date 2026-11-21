//! A bundle read from media: its manifest, its detached signature and the publisher certificate.
//!
//! [`OfflineBundle::read`] reads the three documents from a media directory and
//! [`OfflineBundle::verify`] checks the signature against a trust anchor, so the import engine
//! and any caller that only wants to inspect a bundle share one reader.

use std::path::Path;

use spot_bundle_signing::{BundleSignature, CertificateChain, SignedCertificate};
use spot_updater::Digest;

use crate::error::AirgapError;
use crate::manifest::{MEDIA_CERTIFICATE, MEDIA_MANIFEST, MEDIA_SIGNATURE, OfflineManifest};
use crate::storage::Storage;
use crate::verify::{BundleTrust, VerifiedBundle, verify_bundle};

/// A signed offline bundle as it is read from media.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineBundle {
    /// What the bundle claims about itself.
    pub manifest: OfflineManifest,
    /// The detached signature over the bundle digest.
    pub signature: BundleSignature,
    /// The publisher certificate the signature chains to.
    pub certificate: SignedCertificate,
}

impl OfflineBundle {
    /// Reads the manifest, signature and certificate from `media`.
    ///
    /// # Errors
    /// [`AirgapError::MediaUnreadable`] for a missing document,
    /// [`AirgapError::ManifestMalformed`] for a bad manifest,
    /// [`AirgapError::SignatureMalformed`] for a bad signature, or
    /// [`AirgapError::CertificateMalformed`] for a bad certificate.
    pub fn read(storage: &dyn Storage, media: &Path) -> Result<Self, AirgapError> {
        let manifest_bytes = read_media(storage, &media.join(MEDIA_MANIFEST))?;
        let manifest = OfflineManifest::from_toml(&String::from_utf8_lossy(&manifest_bytes))?;
        let signature_bytes = read_media(storage, &media.join(MEDIA_SIGNATURE))?;
        let signature = BundleSignature::from_json(&signature_bytes)?;
        let certificate_bytes = read_media(storage, &media.join(MEDIA_CERTIFICATE))?;
        let certificate = SignedCertificate::from_json(&certificate_bytes).map_err(|error| {
            AirgapError::CertificateMalformed {
                detail: error.to_string(),
            }
        })?;
        Ok(Self {
            manifest,
            signature,
            certificate,
        })
    }

    /// The bundle digest the signature covers.
    #[must_use]
    pub fn digest(&self) -> Digest {
        self.manifest.digest()
    }

    /// The certificate chain, for verification.
    #[must_use]
    pub fn chain(&self) -> CertificateChain {
        CertificateChain::new(self.certificate.clone())
    }

    /// Checks the bundle's signature and trust.
    ///
    /// # Errors
    /// The first failing check's [`AirgapError`].
    pub fn verify(&self, trust: &BundleTrust<'_>) -> Result<VerifiedBundle, AirgapError> {
        verify_bundle(&self.manifest, &self.signature, &self.chain(), trust)
    }
}

/// Reads a media file, reporting a missing or unreadable one as
/// [`AirgapError::MediaUnreadable`].
pub(crate) fn read_media(storage: &dyn Storage, path: &Path) -> Result<Vec<u8>, AirgapError> {
    storage
        .read(path)
        .map_err(|_| AirgapError::MediaUnreadable {
            path: path.display().to_string(),
        })
}
