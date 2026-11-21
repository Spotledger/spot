//! The manifest: what a backup holds, how to verify it, and who can open it.
//!
//! One manifest describes one backup: the tenant, the artefact's name, size and digest,
//! the chunk layout with each chunk's ciphertext digest, the state (`pending`,
//! `verified`, `unverified`), and the data key wrapped under the tenant recovery key
//! (plus any escrow wrap the customer granted).
//!
//! The manifest is **signed** with HMAC-SHA256 under a key derived from the backup's data
//! key, so only a holder of the recovery key can produce a manifest that opens. The
//! signed bytes are the JSON form with `signature` omitted; field order is the struct's
//! declaration order, which is stable for a given build.
//!
//! Manifests are immutable objects too. A state change (upload finished, verification
//! failed) writes a new manifest **version** under a new key; the highest version is the
//! backup's current state. Nothing is ever overwritten.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use spot_secrets::{KeyProvider, SecretError, WrappedKey};

use crate::crypto::{self, DataKey, KEY_LEN};
use crate::error::{CloudBackupError, Result};

/// The manifest format version this crate writes and the only one it reads.
pub const MANIFEST_FORMAT: u32 = 1;
/// The signature algorithm recorded in every manifest.
pub const SIGNATURE_ALGORITHM: &str = "hmac-sha256";

/// Where one chunk lives and how to check it without the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    /// Zero-based chunk index.
    pub index: u32,
    /// Offset of the chunk's plaintext in the artefact.
    pub offset: u64,
    /// Plaintext length of the chunk.
    pub plain_len: u64,
    /// Ciphertext length of the chunk, in bytes.
    pub cipher_len: u64,
    /// Lowercase hex SHA-256 of the ciphertext.
    pub sha256: String,
    /// The object key this chunk version lives under.
    pub object: String,
}

/// The artefact a backup was taken from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// The artefact's name, as the backup source gave it.
    pub name: String,
    /// Plaintext length, in bytes.
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the plaintext artefact.
    pub sha256: String,
}

/// A wrapped data key: the recovery key's wrap, or one escrow wrap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestKey {
    /// `recovery` or the escrow label the customer gave.
    pub label: String,
    /// The wrapping key id (`tenant-recovery` for the recovery key).
    pub key_id: String,
    /// The wrapping key version.
    pub key_version: u32,
    /// The wrapped key bytes, base64url without padding.
    pub wrapped: String,
}

impl ManifestKey {
    /// Wraps a `spot-secrets` envelope result for the manifest.
    pub fn from_wrapped(label: impl Into<String>, wrapped: &WrappedKey) -> Self {
        Self {
            label: label.into(),
            key_id: wrapped.key.id.clone(),
            key_version: wrapped.key.version,
            wrapped: URL_SAFE_NO_PAD.encode(&wrapped.bytes),
        }
    }

    /// The `spot-secrets` form of this wrap.
    ///
    /// # Errors
    /// [`CloudBackupError::ManifestInvalid`] when the base64 text or the key id is
    /// malformed (a hand-edited manifest).
    pub fn to_wrapped(&self, backup_id: &str) -> Result<WrappedKey> {
        let bytes = URL_SAFE_NO_PAD.decode(&self.wrapped).map_err(|_| {
            CloudBackupError::ManifestInvalid {
                backup_id: backup_id.to_owned(),
                reason: format!("the {} key wrap is not base64url", self.label),
            }
        })?;
        if self.key_id.is_empty() {
            return Err(CloudBackupError::ManifestInvalid {
                backup_id: backup_id.to_owned(),
                reason: format!("the {} key wrap has no key id", self.label),
            });
        }
        Ok(WrappedKey {
            key: spot_secrets::KeyRef {
                id: self.key_id.clone(),
                version: self.key_version,
            },
            bytes,
        })
    }
}

/// The verification state of a backup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BackupState {
    /// Uploaded, not yet verified (or verification is about to run).
    Pending,
    /// Every digest and every sampled chunk verified.
    Verified,
    /// Verification failed after the retry budget; the backup is never restorable as
    /// verified and the reason says what failed.
    Unverified {
        /// What failed, in plain language and without secrets.
        reason: String,
    },
}

impl BackupState {
    /// Whether this state may be restored without an explicit override.
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified)
    }

    /// A short stable name for reports (`pending`, `verified`, `unverified`).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Unverified { .. } => "unverified",
        }
    }
}

/// The manifest's authenticator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSignature {
    /// The algorithm; always [`SIGNATURE_ALGORITHM`].
    pub algorithm: String,
    /// Lowercase hex HMAC-SHA256 of the unsigned manifest bytes.
    pub tag: String,
}

/// One backup's signed manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    /// Manifest format version; [`MANIFEST_FORMAT`].
    pub format: u32,
    /// Immutable version of this backup's manifest, starting at 1.
    pub version: u32,
    /// The backup id.
    pub backup_id: String,
    /// The tenant (site) that owns the backup.
    pub tenant: String,
    /// When the backup was taken, as Unix seconds (time is always an argument).
    pub created_at: i64,
    /// The artefact the backup holds.
    pub source: SourceInfo,
    /// The chunk size the artefact was split at.
    pub chunk_size: u64,
    /// The chunks, in index order.
    pub chunks: Vec<ChunkEntry>,
    /// The verification state.
    pub state: BackupState,
    /// The data key, wrapped for the recovery key and any escrow key.
    pub keys: Vec<ManifestKey>,
    /// The authenticator; absent only before [`BackupManifest::sign`] runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<ManifestSignature>,
}

impl BackupManifest {
    /// Builds a unsigned manifest in the pending state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backup_id: impl Into<String>,
        tenant: impl Into<String>,
        created_at: i64,
        source: SourceInfo,
        chunk_size: u64,
        chunks: Vec<ChunkEntry>,
        keys: Vec<ManifestKey>,
    ) -> Self {
        Self {
            format: MANIFEST_FORMAT,
            version: 1,
            backup_id: backup_id.into(),
            tenant: tenant.into(),
            created_at,
            source,
            chunk_size,
            chunks,
            state: BackupState::Pending,
            keys,
            signature: None,
        }
    }

    /// The object key of one chunk generation.
    ///
    /// A repair after a corrupt read uploads the same plaintext as a new generation, so
    /// the object key is new and the store never overwrites anything. The latest
    /// manifest version names the generation it trusts.
    pub fn chunk_object(tenant: &str, backup_id: &str, index: u32, generation: u32) -> String {
        format!("{tenant}/{backup_id}/chunk-{index:08}-g{generation:04}.bin")
    }

    /// The object key of one manifest version.
    pub fn manifest_key(tenant: &str, backup_id: &str, version: u32) -> String {
        format!("{tenant}/{backup_id}/manifest-{version:04}.json")
    }

    /// The prefix that holds every object of one backup.
    pub fn backup_prefix(tenant: &str, backup_id: &str) -> String {
        format!("{tenant}/{backup_id}/")
    }

    /// The prefix that holds every object of one tenant.
    pub fn tenant_prefix(tenant: &str) -> String {
        format!("{tenant}/")
    }

    /// The current manifest's object key.
    pub fn key(&self) -> String {
        Self::manifest_key(&self.tenant, &self.backup_id, self.version)
    }

    /// The plaintext length of the artefact the manifest describes.
    pub fn plain_bytes(&self) -> u64 {
        self.chunks.iter().map(|chunk| chunk.plain_len).sum()
    }

    /// The ciphertext length of every chunk together.
    pub fn cipher_bytes(&self) -> u64 {
        self.chunks.iter().map(|chunk| chunk.cipher_len).sum()
    }

    /// The manifest key with `label`, if the customer granted it.
    pub fn key_with_label(&self, label: &str) -> Option<&ManifestKey> {
        self.keys.iter().find(|key| key.label == label)
    }

    /// The unsigned bytes: the JSON form with the signature omitted.
    ///
    /// # Errors
    /// [`CloudBackupError::ManifestInvalid`] when serde cannot render the manifest
    /// (which it can for every value this crate constructs).
    pub fn signature_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature = None;
        serde_json::to_vec(&unsigned).map_err(|error| CloudBackupError::ManifestInvalid {
            backup_id: self.backup_id.clone(),
            reason: format!("the manifest cannot be serialised: {error}"),
        })
    }

    /// Signs the manifest in place with the backup's data key.
    ///
    /// # Errors
    /// [`CloudBackupError::ManifestInvalid`] when serialisation fails.
    pub fn sign(&mut self, data_key: &DataKey) -> Result<()> {
        let tag = crypto::hmac_sha256(
            crypto::manifest_key(data_key).as_ref(),
            &self.signature_bytes()?,
        );
        self.signature = Some(ManifestSignature {
            algorithm: SIGNATURE_ALGORITHM.to_owned(),
            tag: crypto::hex(&tag),
        });
        Ok(())
    }

    /// Checks the authenticator under `data_key`.
    ///
    /// # Errors
    /// [`CloudBackupError::ManifestSignature`] when the signature is absent, names
    /// another algorithm or does not verify.
    pub fn verify_signature(&self, data_key: &DataKey) -> Result<()> {
        let Some(signature) = &self.signature else {
            return Err(CloudBackupError::ManifestSignature {
                backup_id: self.backup_id.clone(),
            });
        };
        if signature.algorithm != SIGNATURE_ALGORITHM {
            return Err(CloudBackupError::ManifestSignature {
                backup_id: self.backup_id.clone(),
            });
        }
        let expected = crypto::hmac_sha256(
            crypto::manifest_key(data_key).as_ref(),
            &self.signature_bytes()?,
        );
        let Some(saved) = crypto::parse_sha256(&signature.tag) else {
            return Err(CloudBackupError::ManifestSignature {
                backup_id: self.backup_id.clone(),
            });
        };
        if !crypto::constant_time_eq(&saved, &expected) {
            return Err(CloudBackupError::ManifestSignature {
                backup_id: self.backup_id.clone(),
            });
        }
        Ok(())
    }

    /// The JSON text stored as an object.
    ///
    /// # Errors
    /// [`CloudBackupError::ManifestInvalid`] when serialisation fails.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|error| CloudBackupError::ManifestInvalid {
            backup_id: self.backup_id.clone(),
            reason: format!("the manifest cannot be serialised: {error}"),
        })
    }

    /// Parses and validates a stored manifest.
    ///
    /// # Errors
    /// [`CloudBackupError::ManifestInvalid`] when the JSON is malformed, the format
    /// version is not [`MANIFEST_FORMAT`], ids are unusable as object keys, the chunk
    /// layout has gaps or overlaps, or a digest is not a SHA-256 hex string.
    pub fn from_json(text: &str) -> Result<Self> {
        let manifest: Self =
            serde_json::from_str(text).map_err(|error| CloudBackupError::ManifestInvalid {
                backup_id: String::new(),
                reason: format!("it is not a manifest: {error}"),
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Checks the invariants every consumer relies on.
    ///
    /// # Errors
    /// [`CloudBackupError::ManifestInvalid`] naming the first broken rule.
    pub fn validate(&self) -> Result<()> {
        let invalid = |reason: String| CloudBackupError::ManifestInvalid {
            backup_id: self.backup_id.clone(),
            reason,
        };
        if self.format != MANIFEST_FORMAT {
            return Err(invalid(format!(
                "format {} is not the supported {MANIFEST_FORMAT}",
                self.format
            )));
        }
        if self.version == 0 {
            return Err(invalid("the manifest version starts at 1".to_owned()));
        }
        if !is_object_segment(&self.backup_id) {
            return Err(invalid(
                "the backup id is not usable as an object key segment".to_owned(),
            ));
        }
        if !is_object_segment(&self.tenant) {
            return Err(invalid(
                "the tenant is not usable as an object key segment".to_owned(),
            ));
        }
        if self.chunk_size == 0 || self.chunk_size > crate::chunk::MAX_CHUNK_SIZE {
            return Err(invalid(
                "the chunk size is outside the accepted range".to_owned(),
            ));
        }
        if !crypto::parse_sha256(&self.source.sha256).is_some() {
            return Err(invalid(
                "the source digest is not a SHA-256 hex string".to_owned(),
            ));
        }
        let mut next_offset = 0u64;
        for (position, chunk) in self.chunks.iter().enumerate() {
            let expected = u32::try_from(position).unwrap_or(u32::MAX);
            if chunk.index != expected {
                return Err(invalid(format!(
                    "chunk {position} is numbered {}: the indices must be contiguous from 0",
                    chunk.index
                )));
            }
            if chunk.offset != next_offset {
                return Err(invalid(format!(
                    "chunk {} starts at {} instead of {next_offset}",
                    chunk.index, chunk.offset
                )));
            }
            if chunk.cipher_len < u64::try_from(crypto::NONCE_LEN + crypto::TAG_LEN).unwrap_or(0) {
                return Err(invalid(format!(
                    "chunk {} is shorter than a nonce and a tag",
                    chunk.index
                )));
            }
            if crypto::parse_sha256(&chunk.sha256).is_none() {
                return Err(invalid(format!(
                    "chunk {} has no SHA-256 digest",
                    chunk.index
                )));
            }
            let expected_prefix = format!("{}/{}/chunk-", self.tenant, self.backup_id);
            if !chunk.object.starts_with(&expected_prefix) || !is_object_key(&chunk.object) {
                return Err(invalid(format!(
                    "chunk {} does not name an object of this backup",
                    chunk.index
                )));
            }
            next_offset = next_offset.saturating_add(chunk.plain_len);
        }
        if next_offset != self.source.bytes {
            return Err(invalid(format!(
                "the chunks cover {next_offset} bytes but the source records {}",
                self.source.bytes
            )));
        }
        if self.key_with_label("recovery").is_none() {
            return Err(invalid("the manifest has no recovery key wrap".to_owned()));
        }
        Ok(())
    }
}

/// Whether a string is safe and non-empty as one object key segment.
pub(crate) fn is_object_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 1024 && key.split('/').all(is_object_segment)
}

/// Whether a string is safe and non-empty as one object key segment.
pub(crate) fn is_object_segment(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text != "."
        && text != ".."
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Wraps `data_key` under `provider` for one backup, returning the manifest's key entry.
///
/// # Errors
/// [`CloudBackupError::Store`] when the provider fails; the message never carries key
/// material.
pub async fn wrap_data_key(
    provider: &dyn KeyProvider,
    tenant: &str,
    backup_id: &str,
    label: &str,
    data_key: &DataKey,
) -> Result<ManifestKey> {
    let aad = crypto::key_aad(tenant, backup_id, label);
    let wrapped = provider
        .wrap(data_key.expose(), &aad)
        .await
        .map_err(|error| CloudBackupError::Store {
            reason: format!("wrapping the data key under {label} failed: {error}"),
        })?;
    Ok(ManifestKey::from_wrapped(label, &wrapped))
}

/// Unwraps the data key with `provider` through the manifest's `label` entry.
///
/// # Errors
/// [`CloudBackupError::WrongKey`] when the provider cannot open the wrap (a different
/// recovery key), [`CloudBackupError::ManifestInvalid`] for a malformed entry, and
/// [`CloudBackupError::Store`] for a provider failure.
pub async fn unwrap_data_key(
    provider: &dyn KeyProvider,
    tenant: &str,
    backup_id: &str,
    label: &str,
    manifest: &BackupManifest,
) -> Result<DataKey> {
    let entry =
        manifest
            .key_with_label(label)
            .ok_or_else(|| CloudBackupError::ManifestInvalid {
                backup_id: backup_id.to_owned(),
                reason: format!("the manifest has no {label} key wrap"),
            })?;
    let wrapped = entry.to_wrapped(&manifest.backup_id)?;
    let aad = crypto::key_aad(tenant, backup_id, label);
    match provider.unwrap(&wrapped.key, &wrapped, &aad).await {
        Ok(bytes) => {
            let key: [u8; KEY_LEN] =
                bytes
                    .expose()
                    .try_into()
                    .map_err(|_| CloudBackupError::ManifestInvalid {
                        backup_id: backup_id.to_owned(),
                        reason: "the unwrapped data key is not 32 bytes".to_owned(),
                    })?;
            Ok(DataKey::from_bytes(key))
        }
        Err(SecretError::Tampered)
        | Err(SecretError::UnknownKey { .. })
        | Err(SecretError::RetiredKey { .. })
        | Err(SecretError::Malformed(_)) => Err(CloudBackupError::WrongKey {
            backup_id: backup_id.to_owned(),
        }),
        // The blind-index errors cannot come from an unwrap; they are provider-side problems
        // if a provider ever reports one, so they take the store path rather than `WrongKey`.
        Err(
            error @ (SecretError::Provider(_)
            | SecretError::IndexKey(_)
            | SecretError::DeriveKey(_)
            | SecretError::UnknownKeyVersion { .. }
            | SecretError::ReservedName(_)),
        ) => Err(CloudBackupError::Store {
            reason: format!("unwrapping the data key failed: {error}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(bytes: &[u8]) -> SourceInfo {
        SourceInfo {
            name: "site.tar".to_owned(),
            bytes: u64::try_from(bytes.len()).unwrap_or(0),
            sha256: crypto::sha256_hex(bytes),
        }
    }

    fn chunks_for(bytes: &[u8], chunk_size: u64) -> Vec<ChunkEntry> {
        crate::chunk::plan(u64::try_from(bytes.len()).unwrap_or(0), chunk_size)
            .expect("plan")
            .into_iter()
            .map(|range| ChunkEntry {
                index: range.index,
                offset: range.offset,
                plain_len: range.len,
                cipher_len: range.len + 40,
                sha256: crypto::sha256_hex(&range.index.to_be_bytes()),
                object: BackupManifest::chunk_object("acme.example", "bk-0001", range.index, 0),
            })
            .collect()
    }

    fn manifest(bytes: &[u8]) -> BackupManifest {
        BackupManifest::new(
            "bk-0001",
            "acme.example",
            1_700_000_000,
            source(bytes),
            4,
            chunks_for(bytes, 4),
            vec![ManifestKey {
                label: "recovery".to_owned(),
                key_id: "tenant-recovery".to_owned(),
                key_version: 1,
                wrapped: URL_SAFE_NO_PAD.encode(b"wrapped"),
            }],
        )
    }

    #[test]
    fn signing_and_checking_round_trips() {
        let key = DataKey::generate().expect("rng");
        let mut signed = manifest(b"abcdefgh");
        signed.sign(&key).expect("sign");
        signed.verify_signature(&key).expect("verify");
        signed.validate().expect("valid");
        let other = DataKey::generate().expect("rng");
        assert_eq!(
            signed.verify_signature(&other).expect_err("wrong").code(),
            "cloud_backup.manifest_signature"
        );
    }

    #[test]
    fn changing_a_field_breaks_the_signature() {
        let key = DataKey::generate().expect("rng");
        let mut signed = manifest(b"abcdefgh");
        signed.sign(&key).expect("sign");
        signed.created_at += 1;
        assert!(signed.verify_signature(&key).is_err());
    }

    #[test]
    fn json_round_trips_and_keeps_the_signature() {
        let key = DataKey::generate().expect("rng");
        let mut signed = manifest(b"abcdefgh");
        signed.sign(&key).expect("sign");
        let parsed = BackupManifest::from_json(&signed.to_json().expect("json")).expect("parse");
        assert_eq!(parsed, signed);
        parsed.verify_signature(&key).expect("verify");
    }

    #[test]
    fn validation_rejects_gaps_and_bad_ids() {
        let mut gaps = manifest(b"abcdefgh");
        gaps.chunks[1].offset = 9;
        assert_eq!(
            gaps.validate().expect_err("gap").code(),
            "cloud_backup.manifest_invalid"
        );
        let mut bad_id = manifest(b"abcdefgh");
        bad_id.backup_id = "../escape".to_owned();
        assert!(bad_id.validate().is_err());
        let mut no_recovery = manifest(b"abcdefgh");
        no_recovery.keys.clear();
        assert!(no_recovery.validate().is_err());
    }

    #[test]
    fn object_keys_are_versioned_and_tenant_scoped() {
        assert_eq!(
            BackupManifest::chunk_object("acme.example", "bk-1", 3, 2),
            "acme.example/bk-1/chunk-00000003-g0002.bin"
        );
        assert_eq!(
            BackupManifest::manifest_key("acme.example", "bk-1", 2),
            "acme.example/bk-1/manifest-0002.json"
        );
        assert_eq!(
            BackupManifest::tenant_prefix("acme.example"),
            "acme.example/"
        );
    }
}
