//! Fixtures for the update tests: one signed manifest, two key seeds, an anchor and a revocation
//! list.
//!
//! House pattern: lint-scoped integration tests may `unwrap` what the test itself built.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use semver::Version;
use spot_updater::{
    Channel, Digest, RevokedUpdateKey, SigningKey, TrustedUpdateKey, UpdateFile, UpdateManifest,
    UpdatePayload, UpdateRevocation, UpdateRevocationBody, UpdateSignature, UpdateTrust,
};

/// The product every fixture names.
pub const PRODUCT: &str = "spotledger";
/// The offered version.
pub const VERSION: &str = "2.0.0";
/// The oldest version that may update directly.
pub const MIN_FROM: &str = "1.0.0";
/// When the update was released.
pub const RELEASED_AT: i64 = 1_700_000_000;
/// The check time.
pub const NOW: i64 = 1_800_000_000;
/// The seed of the key that signs the manifest.
pub const SIGNER: u8 = 1;
/// The seed of the key that signs the revocation list.
pub const REVOKER: u8 = 9;

/// The payload's bytes.
pub fn payload_bytes() -> Vec<u8> {
    b"spotledger-update-payload-2.0.0".to_vec()
}

/// One file's bytes.
pub fn file_bytes() -> Vec<u8> {
    b"binary-content".to_vec()
}

/// A key from a fixed seed.
pub fn key(seed: u8) -> SigningKey {
    SigningKey::from_seed(&[seed; 32])
}

/// The version the fixtures offer.
pub fn version() -> Version {
    Version::parse(VERSION).unwrap()
}

/// The good manifest: version 2.0.0, schema epoch 3, one payload and one file.
pub fn manifest() -> UpdateManifest {
    UpdateManifest {
        product: PRODUCT.to_owned(),
        channel: Channel::Stable,
        version: version(),
        min_from: Version::parse(MIN_FROM).unwrap(),
        via: Vec::new(),
        released_at: RELEASED_AT,
        payload: UpdatePayload {
            file: "spotledger-2.0.0.zip".to_owned(),
            size: payload_bytes().len() as u64,
            sha256: Digest::of(&payload_bytes()),
        },
        files: vec![UpdateFile {
            path: "bin/spotledger.exe".to_owned(),
            size: file_bytes().len() as u64,
            sha256: Digest::of(&file_bytes()),
        }],
        migrations: Vec::new(),
        requires_os: Some("Windows 10".to_owned()),
        schema_epoch: 3,
        notes: Some("Bug fixes".to_owned()),
    }
}

/// A trust anchor over the signing key and the revocation key, both with no window ends.
pub fn anchor() -> UpdateTrust {
    anchor_between((0, None), (0, None))
}

/// A trust anchor with explicit windows: `(valid_from, valid_to)` for the signing key and the
/// revocation key.
pub fn anchor_between(signer: (i64, Option<i64>), revoker: (i64, Option<i64>)) -> UpdateTrust {
    UpdateTrust::new(vec![
        TrustedUpdateKey::new(key(SIGNER).public(), signer.0, signer.1),
        TrustedUpdateKey::new(key(REVOKER).public(), revoker.0, revoker.1),
    ])
}

/// A signature of `manifest`'s signing message by the given seed.
pub fn sign(manifest: &UpdateManifest, seed: u8) -> UpdateSignature {
    let signing = key(seed);
    UpdateSignature::from_parts(
        signing.public().key_id(),
        signing.sign(&manifest.signing_message()),
    )
}

/// A revocation list signed by the revocation key.
pub fn revocation(generation: u64, keys: Vec<RevokedUpdateKey>) -> UpdateRevocation {
    let signing = key(REVOKER);
    UpdateRevocation::issue(
        UpdateRevocationBody {
            generation,
            issued_at: 0,
            issued_by: signing.public().key_id(),
            keys,
        },
        &signing,
    )
}

/// An empty revocation list at generation `generation`.
pub fn empty_revocation(generation: u64) -> UpdateRevocation {
    revocation(generation, Vec::new())
}

/// A revoked key naming the signing key.
pub fn revoked_signer() -> RevokedUpdateKey {
    RevokedUpdateKey {
        key_id: key(SIGNER).public().key_id(),
        reason: "compromised".to_owned(),
    }
}

/// The error code of a refusal, or panics when the check passed.
pub fn refused_with<T: std::fmt::Debug>(
    result: Result<T, spot_updater::UpdaterError>,
) -> &'static str {
    match result {
        Ok(value) => panic!("expected a refusal, the check passed: {value:?}"),
        Err(error) => error.code().as_str(),
    }
}
