//! Splitting an artefact into fixed-size chunks and sealing each one.
//!
//! The chunk size is fixed at 4 MiB to start (the brief allows content-defined chunks
//! later; nothing in the manifest depends on how the boundaries were chosen). A chunk is
//! sealed with the backup's data key and its associated data binds the backup id and the
//! chunk index, so a chunk cannot be moved to another backup or swapped with a neighbour
//! without failing to open. The manifest records each chunk's plaintext length, offset
//! and SHA-256 digest of the **ciphertext**, so a digest check needs no key at all.

use zeroize::Zeroizing;

use crate::crypto::{self, DataKey};
use crate::error::{CloudBackupError, Result};

/// The default chunk size: 4 MiB.
pub const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
/// The largest chunk size accepted, to bound a single object and the retry cost.
pub const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// One chunk's place in the artefact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkRange {
    /// Zero-based chunk index.
    pub index: u32,
    /// Offset of the chunk's first plaintext byte in the artefact.
    pub offset: u64,
    /// Plaintext length of the chunk, in bytes.
    pub len: u64,
}

/// Plans the fixed-size chunk ranges of an artefact `len` bytes long.
///
/// An empty artefact is one empty chunk, so every backup has at least one object and a
/// manifest never looks like a missing upload.
///
/// # Errors
/// [`CloudBackupError::ManifestInvalid`] when `chunk_size` is zero, above
/// [`MAX_CHUNK_SIZE`], or when the artefact would need more than [`u32::MAX`] chunks.
pub fn plan(len: u64, chunk_size: u64) -> Result<Vec<ChunkRange>> {
    if chunk_size == 0 || chunk_size > MAX_CHUNK_SIZE {
        return Err(CloudBackupError::ManifestInvalid {
            backup_id: String::new(),
            reason: format!("the chunk size must be between 1 and {MAX_CHUNK_SIZE} bytes"),
        });
    }
    let count = if len == 0 {
        1
    } else {
        len.div_ceil(chunk_size)
    };
    if count > u64::from(u32::MAX) {
        return Err(CloudBackupError::ManifestInvalid {
            backup_id: String::new(),
            reason: "the artefact needs more chunks than a manifest can index".to_owned(),
        });
    }
    let mut ranges = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for index in 0..count {
        let offset = index * chunk_size;
        let remaining = len.saturating_sub(offset);
        let chunk_len = remaining.min(chunk_size);
        ranges.push(ChunkRange {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            offset,
            len: chunk_len,
        });
    }
    Ok(ranges)
}

/// One sealed chunk on its way to the store.
#[derive(Clone, Debug)]
pub struct EncryptedChunk {
    /// Zero-based chunk index.
    pub index: u32,
    /// Offset of the chunk's plaintext in the artefact.
    pub offset: u64,
    /// Plaintext length.
    pub plain_len: u64,
    /// `nonce || ciphertext || tag`.
    pub bytes: Vec<u8>,
    /// SHA-256 of `bytes`.
    pub digest: [u8; 32],
}

impl EncryptedChunk {
    /// The digest as lowercase hex, the manifest's spelling.
    pub fn digest_hex(&self) -> String {
        crypto::hex(&self.digest)
    }

    /// The ciphertext length, in bytes.
    pub fn cipher_len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }
}

/// Splits `artefact` into chunks and seals each one under `data_key`.
///
/// # Errors
/// [`CloudBackupError::ManifestInvalid`] for a bad chunk size, and
/// [`CloudBackupError::Store`] when the AEAD or the RNG refuses.
pub fn seal(
    backup_id: &str,
    data_key: &DataKey,
    artefact: &[u8],
    chunk_size: u64,
) -> Result<Vec<EncryptedChunk>> {
    let ranges = plan(
        u64::try_from(artefact.len()).unwrap_or(u64::MAX),
        chunk_size,
    )?;
    let mut chunks = Vec::with_capacity(ranges.len());
    for range in ranges {
        let start = usize::try_from(range.offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::try_from(range.len).unwrap_or(usize::MAX));
        let plain = artefact
            .get(start..end)
            .ok_or_else(|| CloudBackupError::ManifestInvalid {
                backup_id: backup_id.to_owned(),
                reason: "the chunk plan runs past the artefact".to_owned(),
            })?;
        let aad = crypto::chunk_aad(backup_id, range.index);
        let bytes = crypto::encrypt(data_key.expose(), &aad, plain).ok_or_else(|| {
            CloudBackupError::Store {
                reason: format!("the AEAD refused chunk {}", range.index),
            }
        })?;
        let digest = crypto::sha256(&bytes);
        chunks.push(EncryptedChunk {
            index: range.index,
            offset: range.offset,
            plain_len: range.len,
            bytes,
            digest,
        });
    }
    Ok(chunks)
}

/// Opens one sealed chunk and checks its digest, then its authentication tag.
///
/// # Errors
/// [`CloudBackupError::ChunkCorrupt`] when the digest or the tag does not match;
/// [`CloudBackupError::ChunkMissing`] when there are no bytes at all.
pub fn open(
    backup_id: &str,
    data_key: &DataKey,
    index: u32,
    saved_digest: &str,
    bytes: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    if bytes.is_empty() {
        return Err(CloudBackupError::ChunkMissing {
            backup_id: backup_id.to_owned(),
            index,
        });
    }
    let Some(expected) = crypto::parse_sha256(saved_digest) else {
        return Err(CloudBackupError::ChunkCorrupt {
            backup_id: backup_id.to_owned(),
            index,
            reason: "the manifest digest is not a SHA-256 hex string",
        });
    };
    if !crypto::constant_time_eq(&crypto::sha256(bytes), &expected) {
        return Err(CloudBackupError::ChunkCorrupt {
            backup_id: backup_id.to_owned(),
            index,
            reason: "the ciphertext digest does not match the manifest",
        });
    }
    let aad = crypto::chunk_aad(backup_id, index);
    crypto::decrypt(data_key.expose(), &aad, bytes).ok_or(CloudBackupError::ChunkCorrupt {
        backup_id: backup_id.to_owned(),
        index,
        reason: "the authentication tag does not verify",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_covers_the_artefact_without_gaps() {
        let ranges = plan(10, 4).expect("plan");
        assert_eq!(ranges.len(), 3);
        assert_eq!(
            ranges[0],
            ChunkRange {
                index: 0,
                offset: 0,
                len: 4
            }
        );
        assert_eq!(
            ranges[1],
            ChunkRange {
                index: 1,
                offset: 4,
                len: 4
            }
        );
        assert_eq!(
            ranges[2],
            ChunkRange {
                index: 2,
                offset: 8,
                len: 2
            }
        );
        let total: u64 = ranges.iter().map(|range| range.len).sum();
        assert_eq!(total, 10);
    }

    #[test]
    fn an_empty_artefact_is_one_empty_chunk() {
        let ranges = plan(0, 4).expect("plan");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].len, 0);
    }

    #[test]
    fn a_bad_chunk_size_is_refused() {
        assert!(plan(10, 0).is_err());
        assert!(plan(10, MAX_CHUNK_SIZE + 1).is_err());
    }

    #[test]
    fn sealed_chunks_round_trip() {
        let key = DataKey::generate().expect("rng");
        let artefact = b"the quick brown fox jumps over the lazy dog";
        let chunks = seal("bk-1", &key, artefact, 8).expect("seal");
        assert_eq!(chunks.len(), 6);
        let mut restored = Vec::new();
        for chunk in &chunks {
            let plain =
                open("bk-1", &key, chunk.index, &chunk.digest_hex(), &chunk.bytes).expect("open");
            restored.extend_from_slice(&plain);
        }
        assert_eq!(restored, artefact);
    }

    #[test]
    fn a_chunk_cannot_be_opened_under_another_backup_or_index() {
        let key = DataKey::generate().expect("rng");
        let chunks = seal("bk-1", &key, b"abcdefgh", 4).expect("seal");
        let digest = chunks[0].digest_hex();
        assert!(open("bk-2", &key, 0, &digest, &chunks[0].bytes).is_err());
        assert!(open("bk-1", &key, 1, &digest, &chunks[0].bytes).is_err());
    }

    #[test]
    fn a_flipped_byte_is_reported_as_corruption() {
        let key = DataKey::generate().expect("rng");
        let chunks = seal("bk-1", &key, b"abcdefgh", 4).expect("seal");
        let mut bytes = chunks[0].bytes.clone();
        bytes[0] ^= 0x01;
        let error = open("bk-1", &key, 0, &chunks[0].digest_hex(), &bytes).expect_err("digest");
        assert_eq!(error.code(), "cloud_backup.chunk_corrupt");
    }
}
