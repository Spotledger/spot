//! The export side: assembling a signed offline bundle on a connected machine.
//!
//! [`BundleBuilder`] reads a release directory, computes every file's size and SHA-256, builds
//! the [`OfflineManifest`], asks a [`Signer`] for the detached bundle signature and the
//! publisher certificate, and writes the media layout:
//!
//! ```text
//! <out>/offline.toml  <out>/offline.sig  <out>/certificate.json  <out>/files/...
//! ```
//!
//! The signing keys never enter this crate: [`Signer`] is the callback the caller implements
//! with its own key material (typically wrapping `spot_bundle_signing::sign_bundle`), and this
//! module only ever sees public documents.

use std::path::Path;

use semver::Version;
use spot_bundle_signing::{BundleSignature, SignedCertificate, SigningError};
use spot_updater::Digest;

use crate::error::AirgapError;
use crate::manifest::{
    BundleKind, MEDIA_CERTIFICATE, MEDIA_FILES_DIR, MEDIA_MANIFEST, MEDIA_SIGNATURE, OfflineFile,
    OfflineManifest,
};
use crate::storage::{Storage, walk_files};

/// The signing callback the caller implements with its own key material.
pub trait Signer {
    /// The publisher certificate the signature chains to, shipped with the bundle.
    fn certificate(&self) -> SignedCertificate;

    /// Signs a bundle digest for `name` at `version`, at `signed_at` (UTC seconds).
    ///
    /// # Errors
    /// The signing crate's refusal when the callback's key does not match its certificate.
    fn sign_bundle(
        &self,
        digest: &spot_bundle::Digest,
        name: &str,
        version: &str,
        signed_at: i64,
    ) -> Result<BundleSignature, SigningError>;
}

/// What a bundle the builder assembles is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSpec {
    /// What the bundle carries.
    pub kind: BundleKind,
    /// The bundle name.
    pub name: String,
    /// The bundle version.
    pub version: Version,
    /// When the bundle is released (UTC seconds).
    pub released_at: i64,
    /// Release notes for an administrator.
    pub notes: Option<String>,
}

/// Assembles one signed offline bundle from a release directory.
pub struct BundleBuilder<'a> {
    spec: BundleSpec,
    signer: &'a dyn Signer,
}

impl<'a> BundleBuilder<'a> {
    /// A builder for `spec`, signing with `signer`.
    #[must_use]
    pub fn new(spec: BundleSpec, signer: &'a dyn Signer) -> Self {
        Self { spec, signer }
    }

    /// Reads `release_dir`, signs the manifest and writes the media layout under `out_dir`,
    /// returning the manifest it wrote.
    ///
    /// # Errors
    /// [`AirgapError::Io`] for a filesystem failure, [`AirgapError::SignerRefused`] when the
    /// callback refuses to sign, or [`AirgapError::SignatureMalformed`] when a written document
    /// cannot be produced.
    pub fn assemble(
        &self,
        storage: &dyn Storage,
        release_dir: &Path,
        out_dir: &Path,
    ) -> Result<OfflineManifest, AirgapError> {
        let mut files = Vec::new();
        for relative in walk_files(storage, release_dir)? {
            let bytes = storage.read(&release_dir.join(&relative))?;
            files.push(OfflineFile {
                path: relative,
                size: bytes.len() as u64,
                sha256: Digest::of(&bytes),
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let manifest = OfflineManifest {
            kind: self.spec.kind,
            name: self.spec.name.clone(),
            version: self.spec.version.clone(),
            released_at: self.spec.released_at,
            notes: self.spec.notes.clone(),
            files,
        };

        let digest = manifest.signing_digest()?;
        let signature = self
            .signer
            .sign_bundle(
                &digest,
                &manifest.name,
                &manifest.version.to_string(),
                manifest.released_at,
            )
            .map_err(|error| AirgapError::SignerRefused {
                detail: error.to_string(),
            })?;
        let certificate = self.signer.certificate();

        storage.create_dir_all(out_dir)?;
        let files_dir = out_dir.join(MEDIA_FILES_DIR);
        storage.create_dir_all(&files_dir)?;
        for file in &manifest.files {
            let target = files_dir.join(&file.path);
            if let Some(parent) = target.parent() {
                storage.create_dir_all(parent)?;
            }
            let bytes = storage.read(&release_dir.join(&file.path))?;
            storage.write_atomic(&target, &bytes)?;
        }
        storage.write_atomic(&out_dir.join(MEDIA_MANIFEST), manifest.to_toml().as_bytes())?;
        storage.write_atomic(
            &out_dir.join(MEDIA_SIGNATURE),
            signature.to_json().as_bytes(),
        )?;
        storage.write_atomic(
            &out_dir.join(MEDIA_CERTIFICATE),
            certificate.to_json().as_bytes(),
        )?;
        Ok(manifest)
    }
}
