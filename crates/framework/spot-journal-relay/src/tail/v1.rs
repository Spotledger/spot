//! Version 1 of the tail chunk (ADR-061 A.2): read, never written (A.7).
//!
//! ```text
//! chunk = "SPOTJTL1" u32(count) { u64(generation) u64(seq) u32(line_len) line }* [32](sum)
//! sum   = SHA-256("spot.journal.tail.v1\0" || every byte before sum)
//! ```
//!
//! Integers are big-endian. Version 1 is unauthenticated and plaintext; it exists only so chunks
//! written before WP-JRN-03.T11 are still read.

use sha2::{Digest, Sha256};
use spot_events_contract::envelope::Position;

use super::error::{ChunkError, Rule};
use super::{Entry, MAGIC_V1};

const DOMAIN: &[u8] = b"spot.journal.tail.v1\0";
const ENTRY_HEAD: usize = 8 + 8 + 4;

fn checksum(body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(body);
    hasher.finalize().into()
}

/// Checks the checksum (A.3 (b)), then parses the body (A.3 (c)). `bytes` is the whole object and
/// starts with the version 1 magic.
pub(super) fn decode(bytes: &[u8]) -> Result<Vec<Entry>, ChunkError> {
    let body_len = bytes.len().checked_sub(32).ok_or(ChunkError::new(
        Rule::B,
        "the object is shorter than 32 bytes",
    ))?;
    let (body, sum) = bytes.split_at(body_len);
    if checksum(body) != sum {
        return Err(ChunkError::new(Rule::B, "the checksum does not match"));
    }
    let bad = |reason| ChunkError::new(Rule::C, reason);
    let rest = body
        .strip_prefix(MAGIC_V1.as_slice())
        .ok_or_else(|| bad("the magic is not version 1"))?;
    let (count, mut rest) = rest
        .split_first_chunk::<4>()
        .ok_or_else(|| bad("the entry count is missing"))?;
    let count = u32::from_be_bytes(*count);
    let mut entries = Vec::new();
    for _ in 0..count {
        if rest.len() < ENTRY_HEAD {
            return Err(bad("an entry is cut short"));
        }
        let (head, tail) = rest.split_at(ENTRY_HEAD);
        let number = |from: usize| -> Result<u64, ChunkError> {
            let slice = head
                .get(from..from + 8)
                .ok_or_else(|| bad("an entry is cut short"))?;
            let array: [u8; 8] = slice.try_into().map_err(|_| bad("an entry is cut short"))?;
            Ok(u64::from_be_bytes(array))
        };
        let generation = number(0)?;
        let seq = number(8)?;
        let len_bytes: [u8; 4] = head
            .get(16..20)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| bad("an entry is cut short"))?;
        let len = usize::try_from(u32::from_be_bytes(len_bytes))
            .map_err(|_| bad("a line length does not fit"))?;
        if tail.len() < len {
            return Err(bad("a line runs past the end"));
        }
        let (line, after) = tail.split_at(len);
        entries.push((Position { generation, seq }, line.to_vec()));
        rest = after;
    }
    if rest.is_empty() {
        Ok(entries)
    } else {
        Err(bad("bytes remain after the last entry"))
    }
}

/// The version 1 bytes of `entries`: for tests only (no version 1 chunk is written, A.7).
#[cfg(test)]
pub(crate) fn encode(entries: &[Entry]) -> Option<Vec<u8>> {
    let count = u32::try_from(entries.len()).ok()?;
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC_V1);
    out.extend_from_slice(&count.to_be_bytes());
    for (position, line) in entries {
        out.extend_from_slice(&position.generation.to_be_bytes());
        out.extend_from_slice(&position.seq.to_be_bytes());
        out.extend_from_slice(&u32::try_from(line.len()).ok()?.to_be_bytes());
        out.extend_from_slice(line);
    }
    let sum = checksum(&out);
    out.extend_from_slice(&sum);
    Some(out)
}
