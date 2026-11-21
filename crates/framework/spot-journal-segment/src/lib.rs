//! The sealed journal segment format (WP-JRN-03.T1, ADR-061 s5).
//!
//! A pure library over bytes: it writes a segment of journal entries, reads one back, checks a
//! chain of segments, and decides when to seal. It touches no storage, key, clock or random
//! source; time is an argument (see [`should_seal`]).
//!
//! # Layout (format version 1, all integers big-endian)
//!
//! ```text
//! segment  = prefix header_body header_sum block* footer_body footer_sum footer_len end_magic
//! prefix   = "SPOTJSEG" u16(version = 1) u32(header_body_len)
//! header   = field(source_id) u64(generation) u64(first_seq) u64(last_seq) u64(entry_count)
//!            u32(n) field(schema_version)*n  [32](prev_hash)
//! header_sum = SHA-256("spot.journal.segment.header.v1\0" || prefix || header_body)
//! block    = u32(compressed_len) u32(uncompressed_len) zstd(entries) [32](block_sum)
//! entries  = { u64(seq) u32(line_len) line }*          (line: one line of UTF-8, no newline)
//! block_sum = SHA-256("spot.journal.segment.block.v1\0" || header_sum || u32(ordinal)
//!                      || block bytes before block_sum)
//! footer   = u32(n_blocks) { u64(first_seq) u64(offset) u64(length) u32(count) }*
//! footer_sum = SHA-256("spot.journal.segment.footer.v1\0" || header_sum || footer_body)
//! footer_len = u32(length of footer_body)            end_magic = "SJND"
//! field    = u32(length) bytes
//! ```
//!
//! * **Checksum choice:** SHA-256 only (no CRC crate): one primitive for block, header, footer
//!   and chain. Block checksums are bound to the header checksum and the block's ordinal, so a
//!   valid block cannot be moved to another place or segment undetected.
//! * **Every byte is covered:** header (header_sum), blocks (block_sum, with the index
//!   requiring blocks to be contiguous from the header's end to the footer's start), footer
//!   (footer_sum), trailer (length and magic are checked, the length selects the footer bytes).
//! * **Chain:** `prev_hash` is the SHA-256 of the previous segment's *entire* bytes
//!   ([`segment_hash`]); the first segment of a source carries [`genesis_hash`].
//! * **One generation per segment;** sequence numbers are contiguous inside a segment. An empty
//!   segment is valid (count 0, first and last 0) but the sealing policy never makes one.
//! * **Block target:** a block closes once it holds at least `block_target` uncompressed bytes
//!   (default 256 KiB), so a block may exceed the target by up to one entry.
//!
//! The manifest, signing, encryption and object storage belong to later tasks.

mod chain;
mod error;
mod format;
mod reader;
mod seal;
mod writer;

#[cfg(test)]
mod tests;

pub use chain::{ChainReport, SegmentBytes, verify_chain, verify_chain_from};
pub use error::{ChainError, SegmentError};
pub use format::{
    END_MAGIC, FORMAT_VERSION, Header, MAGIC, MAX_BLOCK_BYTES, MAX_LINE_BYTES, genesis_hash,
    segment_hash,
};
pub use reader::{BlockIndexEntry, ByteSource, Entry, SegmentReader};
pub use seal::{SealLimits, SealState, should_seal};
pub use writer::{
    DEFAULT_BLOCK_TARGET, DEFAULT_ZSTD_LEVEL, SealedSegment, SegmentConfig, SegmentWriter,
};
