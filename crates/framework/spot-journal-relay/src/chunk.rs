//! The tail chunk: the immutable object a remote-sync batch is made durable in before it is
//! acknowledged, compacted into a segment later.
//!
//! ```text
//! chunk = "SPOTJTL1" u32(count) { u64(generation) u64(seq) u32(line_len) line }* [32](sum)
//! sum   = SHA-256("spot.journal.tail.v1\0" || every byte before sum)
//! ```
//!
//! Integers are big-endian. The checksum is what lets `resume` tell a damaged chunk from a whole
//! one; the lines are opaque bytes and never appear in an error.

use sha2::{Digest, Sha256};
use spot_events_contract::envelope::Position;

const MAGIC: &[u8; 8] = b"SPOTJTL1";
const DOMAIN: &[u8] = b"spot.journal.tail.v1\0";
const ENTRY_HEAD: usize = 8 + 8 + 4;

/// One entry of a batch: its position and its envelope line.
pub type Entry = (Position, Vec<u8>);

fn checksum(body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(body);
    hasher.finalize().into()
}

/// The chunk bytes of `entries`, or `None` when an entry line is longer than a `u32` or there are
/// more entries than a `u32` counts.
pub(crate) fn encode(entries: &[Entry]) -> Option<Vec<u8>> {
    let count = u32::try_from(entries.len()).ok()?;
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
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

/// Decodes a chunk; `None` for anything that is not exactly a chunk `encode` produced.
pub(crate) fn decode(bytes: &[u8]) -> Option<Vec<Entry>> {
    let body_len = bytes.len().checked_sub(32)?;
    let (body, sum) = bytes.split_at(body_len);
    if checksum(body) != sum {
        return None;
    }
    let rest = body.strip_prefix(MAGIC.as_slice())?;
    let (count, mut rest) = rest.split_first_chunk::<4>()?;
    let count = u32::from_be_bytes(*count);
    let mut entries = Vec::new();
    for _ in 0..count {
        if rest.len() < ENTRY_HEAD {
            return None;
        }
        let (head, tail) = rest.split_at(ENTRY_HEAD);
        let generation = u64::from_be_bytes(head.get(0..8)?.try_into().ok()?);
        let seq = u64::from_be_bytes(head.get(8..16)?.try_into().ok()?);
        let len = usize::try_from(u32::from_be_bytes(head.get(16..20)?.try_into().ok()?)).ok()?;
        if tail.len() < len {
            return None;
        }
        let (line, after) = tail.split_at(len);
        entries.push((Position { generation, seq }, line.to_vec()));
        rest = after;
    }
    rest.is_empty().then_some(entries)
}
