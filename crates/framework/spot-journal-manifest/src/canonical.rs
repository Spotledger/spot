//! The canonical byte encoding documented in the crate root: encoder, decoder and hash.
//!
//! Every integer is big-endian and two's complement for `i64`; every variable-length field is a
//! `u32` length prefix followed by its bytes; every hash is a fixed 32 bytes with no prefix. The
//! decoder rejects any byte string that does not re-encode to itself, so one manifest has exactly
//! one canonical form.

use sha2::{Digest, Sha256};

use crate::error::ManifestError;
use crate::manifest::{Fork, Manifest, SegmentSummary};

/// The domain tag that opens every manifest's canonical bytes, and the encoding's version.
pub const MANIFEST_TAG: &[u8] = b"spot.journal.manifest.v1\0";

/// The bytes one segment summary occupies: four `u64` fields and two hashes.
const SEGMENT_BYTES: usize = 4 * 8 + 2 * 32;

/// The smallest number of bytes one fork record occupies: two `u64`, an empty text field and an
/// `i64`.
const FORK_BYTES: usize = 8 + 8 + 4 + 8;

/// A `usize` as the encoding's `u32` count.
///
/// A count above [`u32::MAX`] is encoded as [`u32::MAX`]; no manifest of that size can exist
/// (each segment alone occupies 96 bytes), so the value is unreachable and saturating keeps the
/// encoder free of a panic.
fn count_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// Appends `bytes` as a length-prefixed field.
pub(crate) fn put_field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&count_u32(bytes.len()).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Appends an optional hash as `0`, or `1` then the 32 bytes.
pub(crate) fn put_opt_hash(out: &mut Vec<u8>, hash: Option<&[u8; 32]>) {
    match hash {
        None => out.push(0),
        Some(bytes) => {
            out.push(1);
            out.extend_from_slice(bytes);
        }
    }
}

/// The canonical bytes of one segment summary.
fn segment_bytes(out: &mut Vec<u8>, segment: &SegmentSummary) {
    out.extend_from_slice(&segment.generation.to_be_bytes());
    out.extend_from_slice(&segment.first_seq.to_be_bytes());
    out.extend_from_slice(&segment.last_seq.to_be_bytes());
    out.extend_from_slice(&segment.entry_count.to_be_bytes());
    out.extend_from_slice(&segment.segment_hash);
    out.extend_from_slice(&segment.previous_hash);
}

/// The canonical bytes of one fork record.
fn fork_bytes(out: &mut Vec<u8>, fork: &Fork) {
    out.extend_from_slice(&fork.generation.to_be_bytes());
    out.extend_from_slice(&fork.forked_from_position.to_be_bytes());
    put_field(out, fork.reason.as_bytes());
    out.extend_from_slice(&fork.at_millis.to_be_bytes());
}

/// The canonical bytes of `manifest`, which are what [`crate::manifest_hash`] hashes and what a
/// signature covers (together with the key id, see
/// [`SignedManifest::canonical_bytes`](crate::SignedManifest::canonical_bytes)).
#[must_use]
pub fn manifest_bytes(manifest: &Manifest) -> Vec<u8> {
    let mut out = MANIFEST_TAG.to_vec();
    put_field(&mut out, manifest.source.as_bytes());
    out.extend_from_slice(&manifest.generation.to_be_bytes());
    out.extend_from_slice(&count_u32(manifest.segments.len()).to_be_bytes());
    for segment in &manifest.segments {
        segment_bytes(&mut out, segment);
    }
    out.extend_from_slice(&count_u32(manifest.forks.len()).to_be_bytes());
    for fork in &manifest.forks {
        fork_bytes(&mut out, fork);
    }
    put_opt_hash(&mut out, manifest.previous_manifest_hash.as_ref());
    out.extend_from_slice(&manifest.issued_at_millis.to_be_bytes());
    out
}

/// `SHA-256` of the canonical bytes of `manifest`.
#[must_use]
pub fn manifest_hash(manifest: &Manifest) -> [u8; 32] {
    Sha256::digest(manifest_bytes(manifest)).into()
}

/// The bytes a signature covers: the manifest's canonical bytes, then the length-prefixed key id.
///
/// The key id is inside the signature, so a signature cannot be moved to a manifest that names
/// another key.
#[must_use]
pub(crate) fn signed_bytes(manifest: &Manifest, key_id: &str) -> Vec<u8> {
    let mut out = manifest_bytes(manifest);
    put_field(&mut out, key_id.as_bytes());
    out
}

/// A cursor over canonical bytes.
pub(crate) struct Reader<'a> {
    /// The bytes being read.
    bytes: &'a [u8],
    /// How many bytes have been consumed.
    at: usize,
}

impl<'a> Reader<'a> {
    /// A reader at the start of `bytes`.
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// The bytes not yet consumed.
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    /// Consumes `n` bytes.
    fn take(&mut self, n: usize) -> Result<&'a [u8], ManifestError> {
        let end = self.at.saturating_add(n);
        if end > self.bytes.len() {
            return Err(ManifestError::FormatInvalid("the bytes end early"));
        }
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    /// Consumes the domain tag.
    fn tag(&mut self) -> Result<(), ManifestError> {
        if self.take(MANIFEST_TAG.len())? != MANIFEST_TAG {
            return Err(ManifestError::FormatInvalid(
                "the domain tag is not a manifest tag",
            ));
        }
        Ok(())
    }

    /// Consumes a `u32` count.
    fn u32(&mut self) -> Result<u32, ManifestError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| ManifestError::FormatInvalid("a count is truncated"))?;
        Ok(u32::from_be_bytes(bytes))
    }

    /// Consumes a `u64`.
    fn u64(&mut self) -> Result<u64, ManifestError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| ManifestError::FormatInvalid("an integer is truncated"))?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// Consumes an `i64`.
    fn i64(&mut self) -> Result<i64, ManifestError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| ManifestError::FormatInvalid("a timestamp is truncated"))?;
        Ok(i64::from_be_bytes(bytes))
    }

    /// Consumes a 32-byte hash.
    fn hash(&mut self) -> Result<[u8; 32], ManifestError> {
        self.take(32)?
            .try_into()
            .map_err(|_| ManifestError::FormatInvalid("a hash is truncated"))
    }

    /// Consumes a length-prefixed field.
    fn field(&mut self) -> Result<&'a [u8], ManifestError> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    /// Consumes a length-prefixed UTF-8 text.
    fn text(&mut self) -> Result<String, ManifestError> {
        let bytes = self.field()?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| ManifestError::FormatInvalid("a text field is not UTF-8"))
    }

    /// Consumes an optional hash.
    fn opt_hash(&mut self) -> Result<Option<[u8; 32]>, ManifestError> {
        match self.take(1)?[0] {
            0 => Ok(None),
            1 => Ok(Some(self.hash()?)),
            _ => Err(ManifestError::FormatInvalid(
                "the optional-hash flag is neither 0 nor 1",
            )),
        }
    }

    /// Refuses trailing bytes.
    fn end(&self) -> Result<(), ManifestError> {
        if self.remaining() > 0 {
            return Err(ManifestError::FormatInvalid(
                "the bytes carry trailing data",
            ));
        }
        Ok(())
    }
}

/// Decodes the canonical bytes of a signed manifest into its manifest, key id and signature.
///
/// The caller proves the bytes are canonical by re-encoding the result; [`crate::SignedManifest`]
/// does exactly that.
pub(crate) fn decode_signed(bytes: &[u8]) -> Result<(Manifest, String, Vec<u8>), ManifestError> {
    let mut reader = Reader::new(bytes);
    reader.tag()?;
    let source = reader.text()?;
    let generation = reader.u64()?;
    let segment_count = reader.u32()?;
    if segment_count as usize > reader.remaining() / SEGMENT_BYTES {
        return Err(ManifestError::FormatInvalid(
            "the segment count exceeds the bytes",
        ));
    }
    let mut segments = Vec::new();
    for _ in 0..segment_count {
        segments.push(SegmentSummary {
            generation: reader.u64()?,
            first_seq: reader.u64()?,
            last_seq: reader.u64()?,
            entry_count: reader.u64()?,
            segment_hash: reader.hash()?,
            previous_hash: reader.hash()?,
        });
    }
    let fork_count = reader.u32()?;
    if fork_count as usize > reader.remaining() / FORK_BYTES {
        return Err(ManifestError::FormatInvalid(
            "the fork count exceeds the bytes",
        ));
    }
    let mut forks = Vec::new();
    for _ in 0..fork_count {
        forks.push(Fork {
            generation: reader.u64()?,
            forked_from_position: reader.u64()?,
            reason: reader.text()?,
            at_millis: reader.i64()?,
        });
    }
    let previous_manifest_hash = reader.opt_hash()?;
    let issued_at_millis = reader.i64()?;
    let key_id = reader.text()?;
    let signature = reader.field()?.to_vec();
    reader.end()?;
    Ok((
        Manifest {
            source,
            generation,
            segments,
            forks,
            previous_manifest_hash,
            issued_at_millis,
        },
        key_id,
        signature,
    ))
}
