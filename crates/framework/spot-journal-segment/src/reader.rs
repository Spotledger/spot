//! The segment reader: verify, iterate, and read a range through the block index.

use crate::error::SegmentError;
use crate::format::{
    Cursor, END_MAGIC, FORMAT_VERSION, Header, INDEX_ROW_LEN, MAGIC, MAX_BLOCK_BYTES,
    MAX_LINE_BYTES, PREFIX_LEN, TRAILER_LEN, block_checksum, footer_checksum, header_checksum,
};
use spot_events_contract::envelope::Position;
use std::borrow::Cow;

/// A random-access source of segment bytes (a byte slice, or a ranged object-store read).
pub trait ByteSource {
    /// Total length in bytes.
    fn len(&self) -> u64;
    /// Whether the source is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Read exactly `len` bytes at `offset`.
    ///
    /// # Errors
    /// `format_invalid` when the range is outside the source.
    fn read_at(&self, offset: u64, len: u64) -> Result<Cow<'_, [u8]>, SegmentError>;
}

fn slice_read(data: &[u8], offset: u64, len: u64) -> Result<Cow<'_, [u8]>, SegmentError> {
    let end = offset
        .checked_add(len)
        .ok_or(SegmentError::FormatInvalid("range overflow"))?;
    match (usize::try_from(offset), usize::try_from(end)) {
        (Ok(a), Ok(b)) => data
            .get(a..b)
            .map(Cow::Borrowed)
            .ok_or(SegmentError::FormatInvalid("truncated")),
        _ => Err(SegmentError::FormatInvalid("range overflow")),
    }
}

impl ByteSource for &[u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read_at(&self, offset: u64, len: u64) -> Result<Cow<'_, [u8]>, SegmentError> {
        slice_read(self, offset, len)
    }
}

impl ByteSource for Vec<u8> {
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }
    fn read_at(&self, offset: u64, len: u64) -> Result<Cow<'_, [u8]>, SegmentError> {
        slice_read(self, offset, len)
    }
}

/// One row of the footer's block index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockIndexEntry {
    /// First sequence number in the block.
    pub first_seq: u64,
    /// Byte offset of the block in the segment.
    pub offset: u64,
    /// Byte length of the block, including its checksum.
    pub length: u64,
    /// Entries in the block.
    pub count: u32,
}

/// One journal entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The feed position.
    pub position: Position,
    /// The envelope as one JSON line, exactly as the writer was given it.
    pub line: String,
}

/// A verified view over one segment.
pub struct SegmentReader<S: ByteSource> {
    src: S,
    header: Header,
    header_sum: [u8; 32],
    index: Vec<BlockIndexEntry>,
}

impl<S: ByteSource> SegmentReader<S> {
    /// Open a segment: check magic, version, the header and footer checksums and the block
    /// index (contiguous blocks that account for every byte and every entry). Blocks are not
    /// read; see [`Self::verify_all`].
    ///
    /// # Errors
    /// `format_invalid`, `unsupported_version` or `checksum_mismatch`.
    pub fn open(src: S) -> Result<Self, SegmentError> {
        let total = src.len();
        let min = (PREFIX_LEN + 32 + TRAILER_LEN) as u64;
        if total < min {
            return Err(SegmentError::FormatInvalid("too short"));
        }
        let prefix = src.read_at(0, PREFIX_LEN as u64)?;
        let mut c = Cursor::new(&prefix);
        if c.take(8)? != MAGIC {
            return Err(SegmentError::FormatInvalid("bad magic"));
        }
        let mut v = [0u8; 2];
        v.copy_from_slice(c.take(2)?);
        let version = u16::from_be_bytes(v);
        if version != FORMAT_VERSION {
            return Err(SegmentError::UnsupportedVersion(version));
        }
        let body_len = u64::from(c.u32()?);
        let head_end = (PREFIX_LEN as u64)
            .checked_add(body_len)
            .ok_or(SegmentError::FormatInvalid("header length"))?;
        if head_end
            .checked_add(32 + TRAILER_LEN as u64)
            .is_none_or(|e| e > total)
        {
            return Err(SegmentError::FormatInvalid("header length"));
        }
        let head = src.read_at(0, head_end)?;
        let sum_bytes = src.read_at(head_end, 32)?;
        let header_sum = header_checksum(&head);
        if sum_bytes.as_ref() != header_sum {
            return Err(SegmentError::ChecksumMismatch("header"));
        }
        let header = Header::decode_body(head.get(PREFIX_LEN..).unwrap_or_default())?;
        let blocks_start = head_end + 32;

        let trailer = src.read_at(total - TRAILER_LEN as u64, TRAILER_LEN as u64)?;
        let mut t = Cursor::new(&trailer);
        let stored_fsum = t.hash()?;
        let footer_len = u64::from(t.u32()?);
        if t.take(4)? != END_MAGIC {
            return Err(SegmentError::FormatInvalid("bad end magic"));
        }
        let footer_end = total - TRAILER_LEN as u64;
        let footer_start = footer_end
            .checked_sub(footer_len)
            .filter(|s| *s >= blocks_start)
            .ok_or(SegmentError::FormatInvalid("footer length"))?;
        let footer = src.read_at(footer_start, footer_len)?;
        if footer_checksum(&header_sum, &footer) != stored_fsum {
            return Err(SegmentError::ChecksumMismatch("footer"));
        }
        let index = parse_index(&footer, &header, blocks_start, footer_start)?;
        Ok(Self {
            src,
            header,
            header_sum,
            index,
        })
    }

    #[cfg(test)]
    pub(crate) fn test_source(&self) -> &S {
        &self.src
    }

    /// The decoded header.
    #[must_use]
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The block index.
    #[must_use]
    pub fn block_index(&self) -> &[BlockIndexEntry] {
        &self.index
    }

    /// Read, check and decode block `i`.
    fn read_block(&self, i: usize) -> Result<Vec<Entry>, SegmentError> {
        let ix = self
            .index
            .get(i)
            .ok_or(SegmentError::FormatInvalid("block out of range"))?;
        let raw = self.src.read_at(ix.offset, ix.length)?;
        let ordinal =
            u32::try_from(i).map_err(|_| SegmentError::FormatInvalid("block out of range"))?;
        let split = raw
            .len()
            .checked_sub(32)
            .ok_or(SegmentError::FormatInvalid("block too short"))?;
        let (body, sum) = raw.split_at(split);
        if block_checksum(&self.header_sum, ordinal, body) != sum {
            return Err(SegmentError::ChecksumMismatch("block"));
        }
        let mut c = Cursor::new(body);
        let clen = usize::try_from(c.u32()?).map_err(|_| SegmentError::FormatInvalid("block"))?;
        let ulen = usize::try_from(c.u32()?).map_err(|_| SegmentError::FormatInvalid("block"))?;
        let compressed = c.take(clen)?;
        if !c.done() || ulen > MAX_BLOCK_BYTES {
            return Err(SegmentError::FormatInvalid("block lengths"));
        }
        let plain = zstd::bulk::decompress(compressed, ulen)
            .map_err(|_| SegmentError::Compression("decompress failed"))?;
        if plain.len() != ulen {
            return Err(SegmentError::FormatInvalid("block size"));
        }
        let mut e = Cursor::new(&plain);
        let mut out = Vec::new();
        let mut seq = ix.first_seq;
        for n in 0..ix.count {
            let got = e.u64()?;
            if got != seq {
                return Err(SegmentError::FormatInvalid("entry sequence"));
            }
            let len =
                usize::try_from(e.u32()?).map_err(|_| SegmentError::FormatInvalid("entry"))?;
            if len > MAX_LINE_BYTES {
                return Err(SegmentError::FormatInvalid("entry too large"));
            }
            let line = std::str::from_utf8(e.take(len)?)
                .map_err(|_| SegmentError::FormatInvalid("entry is not UTF-8"))?;
            if line.contains(['\n', '\r']) {
                return Err(SegmentError::FormatInvalid("entry has a line break"));
            }
            out.push(Entry {
                position: Position {
                    generation: self.header.generation,
                    seq,
                },
                line: line.to_owned(),
            });
            if n + 1 < ix.count {
                seq = seq
                    .checked_add(1)
                    .ok_or(SegmentError::FormatInvalid("entry sequence"))?;
            }
        }
        if !e.done() {
            return Err(SegmentError::FormatInvalid("trailing bytes in block"));
        }
        Ok(out)
    }

    /// Check every block (checksum, decompression, entry framing) without keeping the entries.
    ///
    /// # Errors
    /// The first failure found.
    pub fn verify_all(&self) -> Result<(), SegmentError> {
        for i in 0..self.index.len() {
            self.read_block(i)?;
        }
        Ok(())
    }

    /// All entries in order, verifying each block as it is reached.
    pub fn entries(&self) -> impl Iterator<Item = Result<Entry, SegmentError>> + '_ {
        let mut next_block = 0usize;
        let mut current: std::vec::IntoIter<Entry> = Vec::new().into_iter();
        let mut failed = false;
        std::iter::from_fn(move || {
            loop {
                if failed {
                    return None;
                }
                if let Some(e) = current.next() {
                    return Some(Ok(e));
                }
                if next_block >= self.index.len() {
                    return None;
                }
                match self.read_block(next_block) {
                    Ok(v) => {
                        current = v.into_iter();
                        next_block += 1;
                    }
                    Err(e) => {
                        failed = true;
                        return Some(Err(e));
                    }
                }
            }
        })
    }

    /// The entries with `first <= seq <= last`, reading and decompressing only the blocks that
    /// overlap the range.
    ///
    /// # Errors
    /// `range_invalid` when `first > last`; block errors for the blocks read.
    pub fn read_range(&self, first: u64, last: u64) -> Result<Vec<Entry>, SegmentError> {
        if first > last {
            return Err(SegmentError::RangeInvalid);
        }
        let mut out = Vec::new();
        for (i, ix) in self.index.iter().enumerate() {
            let block_last = ix.first_seq + u64::from(ix.count) - 1;
            if block_last < first || ix.first_seq > last {
                continue;
            }
            out.extend(
                self.read_block(i)?
                    .into_iter()
                    .filter(|e| e.position.seq >= first && e.position.seq <= last),
            );
        }
        Ok(out)
    }
}

fn parse_index(
    footer: &[u8],
    header: &Header,
    blocks_start: u64,
    footer_start: u64,
) -> Result<Vec<BlockIndexEntry>, SegmentError> {
    let mut c = Cursor::new(footer);
    let n = c.u32()?;
    let n_usize = usize::try_from(n).map_err(|_| SegmentError::FormatInvalid("index"))?;
    if footer.len()
        != 4 + n_usize
            .checked_mul(INDEX_ROW_LEN)
            .ok_or(SegmentError::FormatInvalid("index"))?
    {
        return Err(SegmentError::FormatInvalid("index length"));
    }
    let mut out = Vec::with_capacity(n_usize);
    let mut at = blocks_start;
    let mut seq = header.first_seq;
    let mut total = 0u64;
    for _ in 0..n {
        let ix = BlockIndexEntry {
            first_seq: c.u64()?,
            offset: c.u64()?,
            length: c.u64()?,
            count: c.u32()?,
        };
        if ix.offset != at || ix.count == 0 || ix.length < 40 || ix.first_seq != seq {
            return Err(SegmentError::FormatInvalid("block index"));
        }
        at = at
            .checked_add(ix.length)
            .ok_or(SegmentError::FormatInvalid("block index"))?;
        seq = seq
            .checked_add(u64::from(ix.count))
            .ok_or(SegmentError::FormatInvalid("block index"))?;
        total += u64::from(ix.count);
        out.push(ix);
    }
    if at != footer_start || total != header.entry_count {
        return Err(SegmentError::FormatInvalid("block index"));
    }
    Ok(out)
}
