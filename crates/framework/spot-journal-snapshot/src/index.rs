//! The signed index (`index.json`) and the verified marker (`verified`).
//!
//! The index is written last, so a snapshot without one is incomplete by construction. Its
//! signature covers a domain tag, the key id and the JSON of the body; the body names the tenant,
//! generation and position, so an index cannot be moved to another prefix.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_journal_rebuild_plan::SnapshotInfo;

use crate::error::SnapshotError;
use crate::sign::{SnapshotKeys, SnapshotSigner};

/// The object name of the index.
pub const INDEX_NAME: &str = concat!("index", ".json");
/// The object name of the verified marker.
pub const VERIFIED_NAME: &str = "verified";
/// The index format version.
pub const FORMAT: u32 = 1;
const INDEX_TAG: &[u8] = b"spot.journal.snapshot.v1\0";
const VERIFIED_TAG: &[u8] = b"spot.journal.snapshot.verified.v1\0";

/// One part as the index records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartEntry {
    /// The part's object name inside the snapshot prefix.
    pub name: String,
    /// Lower-case hex SHA-256 of the stored bytes.
    pub sha256: String,
    /// The images in the part.
    pub images: u64,
    /// The bytes before compression (also the decompression limit).
    pub plain_bytes: u64,
    /// The bytes as stored.
    pub stored_bytes: u64,
}

/// The signed content of the index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexBody {
    /// The format version.
    pub format: u32,
    /// The tenant.
    pub tenant: String,
    /// The generation.
    pub generation: u64,
    /// The position every image is as of.
    pub position: u64,
    /// When the snapshot was taken (milliseconds since the epoch), supplied by the caller.
    pub taken_at_millis: i64,
    /// Lower-case hex hash of the manifest the snapshot was taken against.
    pub manifest_hash: String,
    /// The images in all parts.
    pub total_images: u64,
    /// The parts in order.
    pub parts: Vec<PartEntry>,
    /// Images per entity.
    pub entity_counts: BTreeMap<String, u64>,
    /// The entity schema versions seen, per entity, ascending.
    pub entity_schemas: BTreeMap<String, Vec<u64>>,
}

/// The index object: the body, the key id and the signature (hex).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedIndex {
    /// The signed body.
    pub body: IndexBody,
    /// The signing key's id.
    pub key_id: String,
    /// Lower-case hex of the signature.
    pub signature: String,
}

/// The verified marker's signed content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarkerBody {
    pub tenant: String,
    pub generation: u64,
    pub position: u64,
    /// SHA-256 of the index object's bytes the rebuild test verified.
    pub index_sha256: String,
}

/// The verified marker object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SignedMarker {
    pub body: MarkerBody,
    pub key_id: String,
    pub signature: String,
}

fn message<T: Serialize>(tag: &[u8], key_id: &str, body: &T) -> Result<Vec<u8>, SnapshotError> {
    let json = serde_json::to_vec(body)
        .map_err(|_| SnapshotError::IndexInvalid("the body cannot be encoded"))?;
    let mut out = tag.to_vec();
    out.extend_from_slice(
        &u32::try_from(key_id.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(key_id.as_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}

pub(crate) fn is_hash(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Signs `body`.
pub(crate) fn sign_index(
    body: IndexBody,
    signer: &dyn SnapshotSigner,
) -> Result<SignedIndex, SnapshotError> {
    let key_id = signer.key_id().to_owned();
    let signature = hex(&signer.sign(&message(INDEX_TAG, &key_id, &body)?));
    Ok(SignedIndex {
        body,
        key_id,
        signature,
    })
}

pub(crate) fn sign_marker(
    body: MarkerBody,
    signer: &dyn SnapshotSigner,
) -> Result<SignedMarker, SnapshotError> {
    let key_id = signer.key_id().to_owned();
    let signature = hex(&signer.sign(&message(VERIFIED_TAG, &key_id, &body)?));
    Ok(SignedMarker {
        body,
        key_id,
        signature,
    })
}

fn signature_ok<T: Serialize>(
    tag: &[u8],
    body: &T,
    key_id: &str,
    signature: &str,
    generation: u64,
    keys: &dyn SnapshotKeys,
) -> Result<(), SnapshotError> {
    let raw = unhex(signature).ok_or(SnapshotError::SignatureInvalid)?;
    if keys.verify(key_id, generation, &message(tag, key_id, body)?, &raw) {
        Ok(())
    } else {
        Err(SnapshotError::SignatureInvalid)
    }
}

impl SignedIndex {
    /// Parses and validates an index found under `(tenant, generation, position)`: format, place,
    /// internal consistency, then the signature.
    pub(crate) fn open(
        bytes: &[u8],
        tenant: &str,
        generation: u64,
        position: u64,
        keys: &dyn SnapshotKeys,
    ) -> Result<Self, SnapshotError> {
        let index: Self = serde_json::from_slice(bytes)
            .map_err(|_| SnapshotError::IndexInvalid("the index is not valid JSON of its shape"))?;
        let body = &index.body;
        if body.format != FORMAT {
            return Err(SnapshotError::IndexInvalid("the format version is unknown"));
        }
        if body.tenant != tenant || body.generation != generation || body.position != position {
            return Err(SnapshotError::IndexMisplaced);
        }
        signature_ok(
            INDEX_TAG,
            body,
            &index.key_id,
            &index.signature,
            generation,
            keys,
        )?;
        index.check_consistent()?;
        Ok(index)
    }

    fn check_consistent(&self) -> Result<(), SnapshotError> {
        let body = &self.body;
        if !is_hash(&body.manifest_hash) || body.parts.iter().any(|p| !is_hash(&p.sha256)) {
            return Err(SnapshotError::IndexInvalid("a hash is not 64 hex digits"));
        }
        let parts: u64 = body.parts.iter().map(|p| p.images).sum();
        let entities: u64 = body.entity_counts.values().sum();
        if parts != body.total_images || entities != body.total_images {
            return Err(SnapshotError::IndexInvalid("the counts disagree"));
        }
        Ok(())
    }

    /// The planner's view of this snapshot.
    pub(crate) fn info(&self, verified: bool) -> SnapshotInfo {
        SnapshotInfo {
            position: self.body.position,
            generation: self.body.generation,
            verified,
            taken_at_millis: self.body.taken_at_millis,
        }
    }

    /// The canonical bytes of the index object.
    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>, SnapshotError> {
        serde_json::to_vec(self)
            .map_err(|_| SnapshotError::IndexInvalid("the index cannot be encoded"))
    }
}

impl SignedMarker {
    /// Whether the marker is signed and names exactly this index.
    pub(crate) fn matches(
        bytes: &[u8],
        index: &SignedIndex,
        index_sha256: &str,
        keys: &dyn SnapshotKeys,
    ) -> bool {
        let Ok(marker) = serde_json::from_slice::<Self>(bytes) else {
            return false;
        };
        let body = &marker.body;
        body.tenant == index.body.tenant
            && body.generation == index.body.generation
            && body.position == index.body.position
            && body.index_sha256 == index_sha256
            && signature_ok(
                VERIFIED_TAG,
                body,
                &marker.key_id,
                &marker.signature,
                body.generation,
                keys,
            )
            .is_ok()
    }
}
