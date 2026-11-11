//! Shared fixtures for the air-gap integration tests: a signing authority, a media builder and
//! an in-memory result sink.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

use semver::Version;
use spot_airgap::storage::MemoryStorage;
use spot_airgap::{
    AuditSegment, BundleKind, BundleSpec, BundleTrust, OfflineFile, OfflineManifest, ResultBundle,
    ResultSink, Signer, StdStorage, import,
};
use spot_bundle_signing::{
    BundleSignature, CertificateChain, KeyPair, PublisherCertificate, RevocationBody,
    RevocationList, SignedCertificate, SigningError, TrustAnchor, TrustedRoot, generate_keypair,
    issue_certificate, issue_revocation_list, sign_bundle,
};
use spot_updater::Digest;

/// A release-signing authority: a root, a publisher certificate and the publisher's key.
pub struct Authority {
    pub root: KeyPair,
    pub publisher: KeyPair,
    pub certificate: SignedCertificate,
    pub anchor: TrustAnchor,
    pub publisher_id: String,
    pub valid_from: i64,
    pub valid_to: i64,
    pub signed_at: i64,
    pub now: i64,
}

impl Authority {
    /// A deterministic authority: root from seed 1, publisher from seed 2.
    #[must_use]
    pub fn new() -> Self {
        let root = generate_keypair(&[1u8; 32]);
        let publisher = generate_keypair(&[2u8; 32]);
        let publisher_id = "spotledger-test".to_owned();
        let valid_from = 1_000;
        let valid_to = 1_000_000;
        let certificate = issue_certificate(
            PublisherCertificate {
                publisher_id: publisher_id.clone(),
                display_name: "Test Publisher".to_owned(),
                key: publisher.public(),
                valid_from,
                valid_to,
                issued_by: root.public().key_id(),
                serial: 1,
            },
            &root,
        );
        let anchor = TrustAnchor::new(vec![TrustedRoot::new(
            root.public(),
            valid_from,
            Some(valid_to),
        )]);
        Self {
            root,
            publisher,
            certificate,
            anchor,
            publisher_id,
            valid_from,
            valid_to,
            signed_at: 2_000,
            now: 3_000,
        }
    }

    /// An authority whose root is not the one the anchor trusts (for the untrusted-signer case).
    #[must_use]
    pub fn other_root() -> TrustAnchor {
        let stranger = generate_keypair(&[9u8; 32]);
        TrustAnchor::new(vec![TrustedRoot::new(
            stranger.public(),
            1_000,
            Some(1_000_000),
        )])
    }

    /// Signs a manifest's digest.
    ///
    /// # Errors
    /// The signing crate's refusal.
    pub fn sign(&self, manifest: &OfflineManifest) -> Result<BundleSignature, SigningError> {
        sign_bundle(
            &manifest.signing_digest().unwrap(),
            &manifest.name,
            &manifest.version.to_string(),
            &self.certificate,
            &self.publisher,
            self.signed_at,
        )
    }

    /// The certificate chain.
    #[must_use]
    pub fn chain(&self) -> CertificateChain {
        CertificateChain::new(self.certificate.clone())
    }

    /// A revocation list with the given generation and revoked keys.
    #[must_use]
    pub fn revocations(
        &self,
        generation: u64,
        keys: Vec<spot_bundle_signing::RevokedKey>,
    ) -> RevocationList {
        issue_revocation_list(
            RevocationBody {
                generation,
                issued_at: self.signed_at,
                issued_by: self.root.public().key_id(),
                keys,
                publishers: Vec::new(),
                bundles: Vec::new(),
            },
            &self.root,
        )
    }

    /// The trust material an import uses, over the given revocation list.
    #[must_use]
    pub fn trust<'a>(&'a self, revocations: &'a RevocationList) -> BundleTrust<'a> {
        BundleTrust {
            anchor: &self.anchor,
            revocations,
            now: self.now,
            held_revocation_generation: 0,
        }
    }
}

/// A signer for the export builder: a certificate and the publisher's key.
pub struct TestSigner {
    pub certificate: SignedCertificate,
    pub key: KeyPair,
}

impl Signer for TestSigner {
    fn certificate(&self) -> SignedCertificate {
        self.certificate.clone()
    }

    fn sign_bundle(
        &self,
        digest: &spot_bundle::Digest,
        name: &str,
        version: &str,
        signed_at: i64,
    ) -> Result<BundleSignature, SigningError> {
        sign_bundle(
            digest,
            name,
            version,
            &self.certificate,
            &self.key,
            signed_at,
        )
    }
}

/// Builds a manifest over `files` (path, bytes).
#[must_use]
pub fn manifest_of(
    kind: BundleKind,
    name: &str,
    version: &str,
    released_at: i64,
    files: &[(&str, &[u8])],
) -> OfflineManifest {
    let mut files: Vec<OfflineFile> = files
        .iter()
        .map(|(path, bytes)| OfflineFile {
            path: (*path).to_owned(),
            size: bytes.len() as u64,
            sha256: Digest::of(bytes),
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    OfflineManifest {
        kind,
        name: name.to_owned(),
        version: Version::parse(version).unwrap(),
        released_at,
        notes: None,
        files,
    }
}

/// Writes a bundle's media layout into `storage` under `media`, returning the manifest.
pub fn seed_media(
    storage: &MemoryStorage,
    media: &Path,
    authority: &Authority,
    manifest: &OfflineManifest,
    files: &[(&str, &[u8])],
) -> OfflineManifest {
    storage.mkdir(media);
    storage.mkdir(&media.join("files"));
    storage.put_file(&media.join("offline.toml"), manifest.to_toml().as_bytes());
    storage.put_file(
        &media.join("offline.sig"),
        authority.sign(manifest).unwrap().to_json().as_bytes(),
    );
    storage.put_file(
        &media.join("certificate.json"),
        authority.certificate.to_json().as_bytes(),
    );
    for (path, bytes) in files {
        let target = media.join("files").join(path);
        storage.mkdir(target.parent().unwrap());
        storage.put_file(&target, bytes);
    }
    manifest.clone()
}

/// Imports a seeded media bundle with a fresh repository.
///
/// # Errors
/// The import's refusal.
pub fn import_media(
    storage: &MemoryStorage,
    media: &Path,
    repo: &Path,
    authority: &Authority,
    revocations: &RevocationList,
) -> Result<spot_airgap::ImportReport, spot_airgap::AirgapError> {
    let local = spot_airgap::LocalRepo::new(repo);
    import(
        storage,
        &spot_airgap::ImportRequest {
            media,
            repo: &local,
            trust: authority.trust(revocations),
            imported_at: authority.now,
        },
    )
}

/// An in-memory result sink for the idempotent-import test.
#[derive(Default)]
pub struct MemoryResultSink {
    entries: BTreeMap<String, (Digest, ResultBundle)>,
}

impl ResultSink for MemoryResultSink {
    fn contains(&self, id: &str) -> Result<Option<Digest>, spot_airgap::AirgapError> {
        Ok(self.entries.get(id).map(|(digest, _)| *digest))
    }

    fn record(
        &mut self,
        id: &str,
        digest: Digest,
        bundle: ResultBundle,
    ) -> Result<(), spot_airgap::AirgapError> {
        self.entries.insert(id.to_owned(), (digest, bundle));
        Ok(())
    }
}

/// Builds an audit segment over some bytes.
#[must_use]
pub fn audit_segment(bytes: &[u8]) -> AuditSegment {
    AuditSegment::of("site-1", 1, 3, bytes)
}

/// The standard storage, for the real-filesystem round trip.
#[must_use]
pub fn std_storage() -> StdStorage {
    StdStorage::new()
}

/// A spec for the export builder.
#[must_use]
pub fn spec(kind: BundleKind, name: &str, version: &str) -> BundleSpec {
    BundleSpec {
        kind,
        name: name.to_owned(),
        version: Version::parse(version).unwrap(),
        released_at: 2_000,
        notes: None,
    }
}
