//! The segment writer.

use crate::error::SegmentError;
use crate::format::{
    END_MAGIC, FORMAT_VERSION, Header, MAGIC, MAX_BLOCK_BYTES, MAX_LINE_BYTES, block_checksum,
    footer_checksum, genesis_hash, header_checksum, segment_hash,
};
use spot_events_contract::envelope::Position;
use std::collections::BTreeSet;

/// Default uncompressed block target: 256 KiB.
pub const DEFAULT_BLOCK_TARGET: usize = 256 * 1024;
/// Default zstd level.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// Configuration of one segment.
#[derive(Clone, Debug)]
pub struct SegmentConfig {
    /// The journal source the segment belongs to.
    pub source_id: String,
    /// The generation of every entry.
    pub generation: u64,
    /// Hash of the previous segment's bytes; [`SegmentConfig::first`] gives the genesis value.
    pub prev_hash: [u8; 32],
    /// A block is closed once it holds at least this many uncompressed bytes.
    pub block_target: usize,
    /// The zstd compression level.
    pub zstd_level: i32,
}

impl SegmentConfig {
    /// A configuration for the first segment of a source (genesis previous hash).
    #[must_use]
    pub fn first(source_id: &str, generation: u64) -> Self {
        Self::after(source_id, generation, genesis_hash(source_id))
    }

    /// A configuration chained to the previous segment's hash.
    #[must_use]
    pub fn after(source_id: &str, generation: u64, prev_hash: [u8; 32]) -> Self {
        Self {
            source_id: source_id.to_owned(),
            generation,
            prev_hash,
            block_target: DEFAULT_BLOCK_TARGET,
            zstd_level: DEFAULT_ZSTD_LEVEL,
        }
    }
}

/// A sealed segment: its bytes, their hash and the decoded header.
#[derive(Clone, Debug)]
pub struct SealedSegment {
    /// The segment bytes.
    pub bytes: Vec<u8>,
    /// SHA-256 of `bytes`; the next segment's `prev_hash`.
    pub hash: [u8; 32],
    /// The header written into the bytes.
    pub header: Header,
}

struct PendingBlock {
    first_seq: u64,
    count: u32,
    uncompressed_len: u32,
    compressed: Vec<u8>,
}

/// Appends entries in position order and seals them into a segment.
pub struct SegmentWriter {
    cfg: SegmentConfig,
    blocks: Vec<PendingBlock>,
    cur: Vec<u8>,
    cur_first: u64,
    cur_count: u32,
    first_seq: Option<u64>,
    last_seq: u64,
    entries: u64,
    bytes: u64,
}

impl SegmentWriter {
    /// Start a segment.
    ///
    /// # Errors
    /// `config_invalid` for an empty source id, a zero or oversized block target, or a level
    /// zstd refuses.
    pub fn new(cfg: SegmentConfig) -> Result<Self, SegmentError> {
        if cfg.source_id.is_empty() {
            return Err(SegmentError::ConfigInvalid("empty source id"));
        }
        if cfg.block_target == 0 || cfg.block_target > MAX_BLOCK_BYTES / 2 {
            return Err(SegmentError::ConfigInvalid("block target out of range"));
        }
        if !zstd::compression_level_range().contains(&cfg.zstd_level) {
            return Err(SegmentError::ConfigInvalid(
                "compression level out of range",
            ));
        }
        Ok(Self {
            cfg,
            blocks: Vec::new(),
            cur: Vec::new(),
            cur_first: 0,
            cur_count: 0,
            first_seq: None,
            last_seq: 0,
            entries: 0,
            bytes: 0,
        })
    }

    /// Entries appended so far.
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.entries
    }

    /// Uncompressed bytes appended so far (for [`crate::SealState`]).
    #[must_use]
    pub fn uncompressed_bytes(&self) -> u64 {
        self.bytes
    }

    /// Append one entry. `line` is the envelope as one serialised JSON line, treated as opaque.
    ///
    /// # Errors
    /// `entry_invalid` for a line that is not valid UTF-8, holds a newline or is too large;
    /// `generation_mismatch`, `position_out_of_order` (including a duplicate) or `position_gap`.
    pub fn push(&mut self, position: Position, line: &[u8]) -> Result<(), SegmentError> {
        if std::str::from_utf8(line).is_err() {
            return Err(SegmentError::EntryInvalid("not valid UTF-8"));
        }
        if line.contains(&b'\n') || line.contains(&b'\r') {
            return Err(SegmentError::EntryInvalid("contains a line break"));
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(SegmentError::EntryInvalid("too large"));
        }
        if position.generation != self.cfg.generation {
            return Err(SegmentError::GenerationMismatch {
                expected: self.cfg.generation,
                offered: position.generation,
            });
        }
        if self.first_seq.is_some() {
            if position.seq <= self.last_seq {
                return Err(SegmentError::PositionOutOfOrder {
                    generation: position.generation,
                    seq: position.seq,
                });
            }
            if self.last_seq.checked_add(1) != Some(position.seq) {
                return Err(SegmentError::PositionGap {
                    generation: position.generation,
                    seq: position.seq,
                });
            }
        }
        if self.cur_count == 0 {
            self.cur_first = position.seq;
        }
        let len = u32::try_from(line.len()).map_err(|_| SegmentError::EntryInvalid("too large"))?;
        self.cur.extend_from_slice(&position.seq.to_be_bytes());
        self.cur.extend_from_slice(&len.to_be_bytes());
        self.cur.extend_from_slice(line);
        self.cur_count += 1;
        self.first_seq.get_or_insert(position.seq);
        self.last_seq = position.seq;
        self.entries += 1;
        self.bytes += 12 + u64::from(len);
        if self.cur.len() >= self.cfg.block_target {
            self.close_block()?;
        }
        Ok(())
    }

    fn close_block(&mut self) -> Result<(), SegmentError> {
        if self.cur_count == 0 {
            return Ok(());
        }
        let uncompressed_len = u32::try_from(self.cur.len())
            .map_err(|_| SegmentError::Compression("block too large"))?;
        let compressed = zstd::bulk::compress(&self.cur, self.cfg.zstd_level)
            .map_err(|_| SegmentError::Compression("compress failed"))?;
        self.blocks.push(PendingBlock {
            first_seq: self.cur_first,
            count: self.cur_count,
            uncompressed_len,
            compressed,
        });
        self.cur.clear();
        self.cur_count = 0;
        Ok(())
    }

    /// Seal the segment. `schema_versions` are the event-schema versions present in the
    /// entries, as the caller knows them (sorted and de-duplicated here).
    ///
    /// # Errors
    /// `compression` when a block cannot be compressed.
    pub fn seal<I, S>(mut self, schema_versions: I) -> Result<SealedSegment, SegmentError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.close_block()?;
        let versions: BTreeSet<String> = schema_versions.into_iter().map(Into::into).collect();
        let header = Header {
            source_id: self.cfg.source_id.clone(),
            generation: self.cfg.generation,
            first_seq: self.first_seq.unwrap_or(0),
            last_seq: if self.first_seq.is_some() {
                self.last_seq
            } else {
                0
            },
            entry_count: self.entries,
            schema_versions: versions.into_iter().collect(),
            prev_hash: self.cfg.prev_hash,
        };
        let body = header.encode_body();
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        out.extend_from_slice(
            &u32::try_from(body.len())
                .map_err(|_| SegmentError::FormatInvalid("header"))?
                .to_be_bytes(),
        );
        out.extend_from_slice(&body);
        let hsum = header_checksum(&out);
        out.extend_from_slice(&hsum);

        let mut footer = Vec::new();
        footer.extend_from_slice(
            &u32::try_from(self.blocks.len())
                .map_err(|_| SegmentError::FormatInvalid("blocks"))?
                .to_be_bytes(),
        );
        for (i, b) in self.blocks.iter().enumerate() {
            let ordinal = u32::try_from(i).map_err(|_| SegmentError::FormatInvalid("blocks"))?;
            let clen = u32::try_from(b.compressed.len())
                .map_err(|_| SegmentError::Compression("block too large"))?;
            let start = out.len();
            out.extend_from_slice(&clen.to_be_bytes());
            out.extend_from_slice(&b.uncompressed_len.to_be_bytes());
            out.extend_from_slice(&b.compressed);
            let sum = block_checksum(&hsum, ordinal, &out[start..]);
            out.extend_from_slice(&sum);
            footer.extend_from_slice(&b.first_seq.to_be_bytes());
            footer.extend_from_slice(&(start as u64).to_be_bytes());
            footer.extend_from_slice(&((out.len() - start) as u64).to_be_bytes());
            footer.extend_from_slice(&b.count.to_be_bytes());
        }
        let fsum = footer_checksum(&hsum, &footer);
        let footer_len =
            u32::try_from(footer.len()).map_err(|_| SegmentError::FormatInvalid("footer"))?;
        out.extend_from_slice(&footer);
        out.extend_from_slice(&fsum);
        out.extend_from_slice(&footer_len.to_be_bytes());
        out.extend_from_slice(END_MAGIC);
        let hash = segment_hash(&out);
        Ok(SealedSegment {
            bytes: out,
            hash,
            header,
        })
    }
}
