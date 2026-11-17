//! The fixed keys: one Ed25519 key signs manifests and snapshots; the key set verifies both.

use ed25519_dalek::SigningKey;
use spot_journal_manifest::{Manifest, SignedManifest, VerifyingKeys, sign};
use spot_journal_relay::ManifestSigner;
use spot_journal_sign::{Ed25519SnapshotSigner, ManifestSnapshotKeys};

pub const KEY_ID: &str = "journal-key-1";

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

pub struct Signer;

impl ManifestSigner for Signer {
    fn sign(&self, manifest: &Manifest) -> SignedManifest {
        sign(manifest, &signing_key(), KEY_ID)
    }
    fn key_id(&self) -> &str {
        KEY_ID
    }
}

pub fn manifest_keys() -> VerifyingKeys {
    let mut keys = VerifyingKeys::new();
    keys.add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
        .expect("key window");
    keys
}

pub fn snapshot_signer() -> Ed25519SnapshotSigner {
    Ed25519SnapshotSigner::new(KEY_ID, signing_key())
}

pub fn snapshot_keys() -> ManifestSnapshotKeys {
    ManifestSnapshotKeys::new(manifest_keys())
}
