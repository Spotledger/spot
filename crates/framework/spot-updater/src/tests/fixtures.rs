//! Shared fixtures for the engine tests: a signed manifest over a real ZIP, the anchor and the
//! revocation list, and the installed state.
#![allow(dead_code)]

use std::io::{Cursor, Write as _};

use semver::Version;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::archive::ArchiveLimits;
use crate::decide::{Installed, UpdatePolicy};
use crate::digest::Digest;
use crate::engine::ApplyRequest;
use crate::layout::Layout;
use crate::manifest::{Channel, UpdateFile, UpdateManifest, UpdatePayload};
use crate::testing::{FakeDataHooks, FakeFileSystem, FakeService};
use crate::trust::{
    SigningKey, TrustedUpdateKey, UpdateRevocation, UpdateRevocationBody, UpdateSignature,
    UpdateTrust,
};

/// The product every fixture names.
pub const PRODUCT: &str = "spotledger";
/// When the update was released.
pub const RELEASED_AT: i64 = 1_700_000_000;
/// The check time.
pub const NOW: i64 = 1_800_000_000;
/// The seed of the key that signs the manifest.
pub const SIGNER: u8 = 1;
/// The seed of the key that signs the revocation list.
pub const REVOKER: u8 = 9;

/// The files of the old version.
pub const OLD_FILES: &[(&str, &[u8])] =
    &[("bin/spot", b"old-binary"), ("share/notice", b"old-notice")];
/// The files of the new version.
pub const NEW_FILES: &[(&str, &[u8])] =
    &[("bin/spot", b"new-binary"), ("share/notice", b"new-notice")];

/// The version installed before the update.
pub fn old_version() -> Version {
    Version::parse("1.0.0").unwrap()
}

/// The version the fixtures offer.
pub fn new_version() -> Version {
    Version::parse("2.0.0").unwrap()
}

/// A key from a fixed seed.
pub fn key(seed: u8) -> SigningKey {
    SigningKey::from_seed(&[seed; 32])
}

/// A trust anchor over the signing key and the revocation key.
pub fn anchor() -> UpdateTrust {
    UpdateTrust::new(vec![
        TrustedUpdateKey::new(key(SIGNER).public(), 0, None),
        TrustedUpdateKey::new(key(REVOKER).public(), 0, None),
    ])
}

/// An empty revocation list at `generation`.
pub fn empty_revocation(generation: u64) -> UpdateRevocation {
    let signing = key(REVOKER);
    UpdateRevocation::issue(
        UpdateRevocationBody {
            generation,
            issued_at: 0,
            issued_by: signing.public().key_id(),
            keys: Vec::new(),
        },
        &signing,
    )
}

/// Builds a ZIP archive in memory from `(name, bytes)` files.
pub fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in files {
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// A manifest over `files` and their `archive`, for `version` at `schema_epoch`.
pub fn manifest_for(
    version: &Version,
    schema_epoch: u64,
    migrations: Vec<u64>,
    files: &[(&str, &[u8])],
    archive: &[u8],
) -> UpdateManifest {
    UpdateManifest {
        product: PRODUCT.to_owned(),
        channel: Channel::Stable,
        version: version.clone(),
        min_from: old_version(),
        via: Vec::new(),
        released_at: RELEASED_AT,
        payload: UpdatePayload {
            file: format!("spotledger-{version}.zip"),
            size: archive.len() as u64,
            sha256: Digest::of(archive),
        },
        files: files
            .iter()
            .map(|(path, bytes)| UpdateFile {
                path: (*path).to_owned(),
                size: bytes.len() as u64,
                sha256: Digest::of(bytes),
            })
            .collect(),
        migrations,
        requires_os: None,
        schema_epoch,
        notes: None,
    }
}

/// Signs `manifest` with the signing key.
pub fn sign(manifest: &UpdateManifest) -> UpdateSignature {
    let signing = key(SIGNER);
    UpdateSignature::from_parts(
        signing.public().key_id(),
        signing.sign(&manifest.signing_message()),
    )
}

/// A complete, ready-to-run update.
pub struct Fixture {
    /// The offered manifest.
    pub manifest: UpdateManifest,
    /// Its signature.
    pub signature: UpdateSignature,
    /// The trust anchor.
    pub trust: UpdateTrust,
    /// The revocation list.
    pub revocations: UpdateRevocation,
    /// The archive bytes.
    pub payload: Vec<u8>,
    /// The installed state.
    pub installed: Installed,
    /// The policy.
    pub policy: UpdatePolicy,
    /// The archive guards.
    pub limits: ArchiveLimits,
}

impl Fixture {
    /// An update from 1.0.0 to 2.0.0 with no schema change.
    pub fn good() -> Self {
        Self::build(2, Vec::new())
    }

    /// An update that changes the schema: the new version needs epoch 3, the installed database
    /// is at 2, and the manifest lists the migration from 2.
    pub fn with_schema_change() -> Self {
        Self::build(3, vec![2])
    }

    fn build(schema_epoch: u64, migrations: Vec<u64>) -> Self {
        Self::from_files(NEW_FILES, schema_epoch, migrations)
    }

    /// A fixture over arbitrary `(path, bytes)` files, no schema change.
    pub fn with_files(files: &[(&str, &[u8])]) -> Self {
        Self::from_files(files, 2, Vec::new())
    }

    fn from_files(files: &[(&str, &[u8])], schema_epoch: u64, migrations: Vec<u64>) -> Self {
        let payload = make_zip(files);
        let manifest = manifest_for(&new_version(), schema_epoch, migrations, files, &payload);
        let signature = sign(&manifest);
        Self {
            manifest,
            signature,
            trust: anchor(),
            revocations: empty_revocation(1),
            payload,
            installed: Installed {
                product: PRODUCT.to_owned(),
                version: old_version(),
                channel: Channel::Stable,
                schema_epoch: 2,
                payload_sha256: None,
            },
            policy: UpdatePolicy::default(),
            limits: ArchiveLimits::default(),
        }
    }

    /// The request the engine runs.
    pub fn request(&self) -> ApplyRequest<'_> {
        ApplyRequest {
            installed: self.installed.clone(),
            manifest: self.manifest.clone(),
            signature: Some(self.signature.clone()),
            trust: &self.trust,
            revocations: &self.revocations,
            held_revocation_generation: 0,
            payload: &self.payload,
            policy: self.policy.clone(),
            limits: self.limits,
        }
    }

    /// A request whose payload has one flipped byte in the archive.
    pub fn tampered_payload(&self) -> Vec<u8> {
        let mut bytes = self.payload.clone();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        bytes
    }
}

/// The install root every test uses.
pub fn layout() -> Layout {
    Layout::new("/opt/spot")
}

/// Seeds a running install at 1.0.0: its version directory and the `current` pointer.
pub fn seed_installed(fs: &FakeFileSystem, layout: &Layout) {
    let version = old_version();
    fs.seed_version(layout, &version, OLD_FILES);
    fs.seed_current(layout, &version);
}

/// A service running the installed version, ready and healthy.
pub fn running_service() -> FakeService {
    FakeService::new(Some(old_version()))
}

/// Hooks whose every step works.
pub fn hooks() -> FakeDataHooks {
    FakeDataHooks::new()
}
