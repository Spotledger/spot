//! Byte-level helpers shared by the writer and the reader: constants, the header, checksums.

use crate::error::SegmentError;
use sha2::{Digest, Sha256};

/// First eight bytes of every segment.
pub const MAGIC: &[u8; 8] = b"SPOTJSEG";
/// Last four bytes of every segment.
pub const END_MAGIC: &[u8; 4] = b"SJND";
/// The format version this crate writes and reads.
pub const FORMAT_VERSION: u16 = 1;
/// Bytes before the header body: magic, version, header body length.
pub(crate) const PREFIX_LEN: usize = 8 + 2 + 4;
/// The trailer after the footer body: footer checksum, footer body length, end magic.
pub(crate) const TRAILER_LEN: usize = 32 + 4 + 4;
/// One footer index row: first seq, offset, length (u64 each) and entry count (u32).
pub(crate) const INDEX_ROW_LEN: usize = 8 + 8 + 8 + 4;
/// The largest uncompressed block a reader will allocate.
pub const MAX_BLOCK_BYTES: usize = 64 * 1024 * 1024;
/// The longest entry line, in bytes.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

const HEADER_DOMAIN: &[u8] = b"spot.journal.segment.header.v1\0";
const FOOTER_DOMAIN: &[u8] = b"spot.journal.segment.footer.v1\0";
const BLOCK_DOMAIN: &[u8] = b"spot.journal.segment.block.v1\0";
const GENESIS_DOMAIN: &[u8] = b"spot.journal.segment.genesis.v1\0";

/// The decoded header of a segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// The journal source (a tenant or site) the segment belongs to.
    pub source_id: String,
    /// The generation of every entry in the segment.
    pub generation: u64,
    /// First sequence number (0 when the segment is empty).
    pub first_seq: u64,
    /// Last sequence number (0 when the segment is empty).
    pub last_seq: u64,
    /// Number of entries.
    pub entry_count: u64,
    /// The event-schema versions present, as given by the caller; sorted, unique.
    pub schema_versions: Vec<String>,
    /// SHA-256 of the previous segment's bytes, or [`genesis_hash`] for the first segment.
    pub prev_hash: [u8; 32],
}

/// The previous-hash value of the first segment of a source.
#[must_use]
pub fn genesis_hash(source_id: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(GENESIS_DOMAIN);
    put_field(&mut h, source_id.as_bytes());
    h.finalize().into()
}

/// The SHA-256 of a segment's bytes: what the next segment's header commits to.
#[must_use]
pub fn segment_hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(crate) trait Absorb {
    fn absorb(&mut self, data: &[u8]);
}
impl Absorb for Sha256 {
    fn absorb(&mut self, data: &[u8]) {
        self.update(data);
    }
}
impl Absorb for Vec<u8> {
    fn absorb(&mut self, data: &[u8]) {
        self.extend_from_slice(data);
    }
}

/// A `u32` length then the bytes. Lengths above `u32::MAX` cannot occur (callers bound them).
pub(crate) fn put_field<A: Absorb>(out: &mut A, data: &[u8]) {
    out.absorb(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.absorb(data);
}

impl Header {
    pub(crate) fn encode_body(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_field(&mut b, self.source_id.as_bytes());
        for v in [
            self.generation,
            self.first_seq,
            self.last_seq,
            self.entry_count,
        ] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(
            &u32::try_from(self.schema_versions.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        for s in &self.schema_versions {
            put_field(&mut b, s.as_bytes());
        }
        b.extend_from_slice(&self.prev_hash);
        b
    }

    pub(crate) fn decode_body(body: &[u8]) -> Result<Self, SegmentError> {
        let mut c = Cursor::new(body);
        let source_id = c.string()?;
        let generation = c.u64()?;
        let first_seq = c.u64()?;
        let last_seq = c.u64()?;
        let entry_count = c.u64()?;
        let n = c.u32()?;
        let mut schema_versions = Vec::new();
        for _ in 0..n {
            schema_versions.push(c.string()?);
        }
        let prev_hash = c.hash()?;
        if !c.done() {
            return Err(SegmentError::FormatInvalid("trailing bytes in header"));
        }
        let sorted = schema_versions.windows(2).all(|w| w[0] < w[1]);
        let range_ok = if entry_count == 0 {
            first_seq == 0 && last_seq == 0
        } else {
            last_seq
                .checked_sub(first_seq)
                .and_then(|d| d.checked_add(1))
                == Some(entry_count)
        };
        if source_id.is_empty() || !sorted || !range_ok {
            return Err(SegmentError::FormatInvalid("inconsistent header"));
        }
        Ok(Self {
            source_id,
            generation,
            first_seq,
            last_seq,
            entry_count,
            schema_versions,
            prev_hash,
        })
    }
}

/// Checksum over everything from the magic to the end of the header body.
pub(crate) fn header_checksum(prefix_and_body: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(HEADER_DOMAIN);
    h.update(prefix_and_body);
    h.finalize().into()
}

/// Footer checksum, bound to the header checksum.
pub(crate) fn footer_checksum(header_sum: &[u8; 32], footer_body: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(FOOTER_DOMAIN);
    h.update(header_sum);
    h.update(footer_body);
    h.finalize().into()
}

/// Block checksum, bound to the header checksum and the block's ordinal, over the block bytes
/// that precede the checksum itself.
pub(crate) fn block_checksum(
    header_sum: &[u8; 32],
    ordinal: u32,
    block_without_sum: &[u8],
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(BLOCK_DOMAIN);
    h.update(header_sum);
    h.update(ordinal.to_be_bytes());
    h.update(block_without_sum);
    h.finalize().into()
}

/// A bounds-checked reader over a byte slice.
pub(crate) struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }
    pub(crate) fn done(&self) -> bool {
        self.at == self.data.len()
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], SegmentError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(SegmentError::FormatInvalid("length overflow"))?;
        let s = self
            .data
            .get(self.at..end)
            .ok_or(SegmentError::FormatInvalid("truncated"))?;
        self.at = end;
        Ok(s)
    }
    pub(crate) fn u32(&mut self) -> Result<u32, SegmentError> {
        let mut a = [0u8; 4];
        a.copy_from_slice(self.take(4)?);
        Ok(u32::from_be_bytes(a))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, SegmentError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_be_bytes(a))
    }
    pub(crate) fn hash(&mut self) -> Result<[u8; 32], SegmentError> {
        let mut a = [0u8; 32];
        a.copy_from_slice(self.take(32)?);
        Ok(a)
    }
    pub(crate) fn field(&mut self) -> Result<&'a [u8], SegmentError> {
        let n = usize::try_from(self.u32()?)
            .map_err(|_| SegmentError::FormatInvalid("length overflow"))?;
        self.take(n)
    }
    pub(crate) fn string(&mut self) -> Result<String, SegmentError> {
        String::from_utf8(self.field()?.to_vec())
            .map_err(|_| SegmentError::FormatInvalid("text is not UTF-8"))
    }
}
