//! Verification: the signature under a trusted key window, then the manifest's own invariants.

use crate::error::ManifestError;
use crate::findings::{Finding, check_against};
use crate::manifest::{Fork, Manifest, SegmentSummary};
use crate::sign::{SignedManifest, VerifyingKeys};

/// A manifest that is authentic (its signature holds under a key trusted for its generation) and
/// internally consistent (it passes [`Manifest::validate`]).
///
/// Only [`verify`] produces one, and it exposes no way to change the manifest afterwards, so the
/// findings [`Verified::check_against`] computes are always about a manifest that verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The verified manifest.
    manifest: Manifest,
    /// The id of the key whose signature held.
    key_id: String,
}

impl Verified {
    /// The manifest.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The source whose journal the manifest describes.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.manifest.source
    }

    /// The generation the manifest describes.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.manifest.generation
    }

    /// The id of the key the manifest was verified under.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Unix milliseconds the manifest was issued at (as the caller supplied them).
    #[must_use]
    pub fn issued_at_millis(&self) -> i64 {
        self.manifest.issued_at_millis
    }

    /// The segments the manifest lists, in position order.
    #[must_use]
    pub fn segments(&self) -> &[SegmentSummary] {
        &self.manifest.segments
    }

    /// The fork history the manifest carries.
    #[must_use]
    pub fn forks(&self) -> &[Fork] {
        &self.manifest.forks
    }

    /// The last sequence number the manifest lists (`0` when it lists nothing).
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.manifest.last_seq()
    }

    /// The differences between this manifest and the segments storage holds; see
    /// [`check_against`].
    #[must_use]
    pub fn check_against(&self, observed: &[SegmentSummary]) -> Vec<Finding> {
        check_against(self, observed)
    }
}

/// Verifies `signed` against `keys` and returns the manifest as a [`Verified`].
///
/// The signature is checked first, so a forged manifest never reaches the invariant checks and a
/// caller cannot read a tampered manifest's defects as if they were an operator's mistake; the
/// manifest's invariants come second, so an authentic but malformed manifest is still refused.
///
/// # Errors
/// [`ManifestError::KeyUnknown`], [`ManifestError::KeyNotValidForGeneration`],
/// [`ManifestError::SignatureInvalid`], then everything [`Manifest::validate`] reports.
pub fn verify(signed: &SignedManifest, keys: &VerifyingKeys) -> Result<Verified, ManifestError> {
    keys.verify_message(
        &signed.key_id,
        signed.manifest.generation,
        &signed.canonical_bytes(),
        &signed.signature,
    )?;
    signed.manifest.validate()?;
    Ok(Verified {
        manifest: signed.manifest.clone(),
        key_id: signed.key_id.clone(),
    })
}
