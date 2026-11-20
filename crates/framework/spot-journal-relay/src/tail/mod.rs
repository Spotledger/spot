//! The tail chunk: the immutable object a `remote-sync` batch is made durable in before it is
//! acknowledged, compacted into a segment later (ADR-061 Appendix A).
//!
//! * Version 1 (`SPOTJTL1`) is plaintext and unauthenticated; it is **read** for chunks already
//!   written and never written again (A.7).
//! * Version 2 (`SPOTJTL2`) holds AEAD ciphertext lines and one HMAC per batch over the bytes
//!   and the object key (A.2.2). It is what [`encode_v2`] writes.
//!
//! [`decode`] applies A.3 (a) to (e) in order, verifying the checksum or MAC before it parses or
//! decrypts anything, and answers a [`ChunkError`] naming the rule that failed. Rule (f), the
//! order across chunks, belongs to the reader that sees several chunks (`Relay::resume`).
//!
//! A second implementation can test against `tests/fixtures/tail/`: one chunk of each version
//! with the decode it must produce.

mod error;
mod keys;
pub(crate) mod v1;
mod v2;

use spot_events_contract::envelope::Position;
use spot_journal_store::{Key, Layout};

pub use error::{ChunkError, EncodeError, Rule};
pub use keys::TailKeys;

/// The magic of version 1.
pub const MAGIC_V1: &[u8; 8] = b"SPOTJTL1";
/// The magic of version 2.
pub const MAGIC_V2: &[u8; 8] = b"SPOTJTL2";
/// The largest tail chunk object, in bytes (A.7, D6): 64 MiB.
pub const MAX_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
/// The sum of line bytes a batcher should stop at, so the sealed form and the framing of a batch
/// stay below [`MAX_CHUNK_BYTES`]. A guide only: [`encode_v2`] measures the real object and the
/// relay refuses an oversize batch.
pub const BATCH_LINE_BUDGET: u64 = 32 * 1024 * 1024;

/// The most a sealed entry adds to its line, in bytes, for a key provider that wraps a 32-byte
/// data key into at most 512 bytes under a key id of at most 64 bytes (the local, environment and
/// in-memory providers wrap into 72 bytes; a KMS wrap is a few hundred). It is the entry head
/// (20) plus the sealed form's framing (4 + 1 + 64 + 4 + 2), the wrapped key (512), the nonce
/// (24) and the tag (16). [`encoded_len_upper_bound`] uses it; [`encode_v2`] still measures the
/// real object, so a provider outside this assumption is refused by the relay, never written
/// oversize.
pub const ENTRY_OVERHEAD_BOUND: u64 = 20 + (4 + 1 + 64 + 4 + 2) + 512 + 24 + 16;
/// The bytes of a version 2 chunk outside its entries: the magic (8), the count (4) and the MAC
/// (32).
pub const CHUNK_FRAMING_BYTES: u64 = 8 + 4 + 32;

/// One entry of a chunk: its position and its envelope line.
pub type Entry = (Position, Vec<u8>);

/// A chunk that satisfies A.3 (a) to (e).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailChunk {
    /// The format version, 1 or 2.
    pub version: u8,
    /// The batch id of the object key.
    pub batch_id: u64,
    /// The entries, in order, never empty.
    pub entries: Vec<Entry>,
}

impl TailChunk {
    /// The first sequence number (a chunk is never empty).
    #[must_use]
    pub fn first_seq(&self) -> u64 {
        self.entries.first().map_or(0, |(position, _)| position.seq)
    }

    /// The last sequence number.
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.entries.last().map_or(0, |(position, _)| position.seq)
    }
}

/// Encodes a version 2 chunk of `entries` for the object `object_key` (`batch_id` is the one in
/// that key). The MAC covers that key, so the chunk can be stored under it only. A chunk over
/// [`MAX_CHUNK_BYTES`] is refused with [`EncodeError::TooLarge`] and nothing is produced.
///
/// # Errors
/// [`EncodeError`].
pub async fn encode_v2(
    keys: &TailKeys,
    object_key: &str,
    batch_id: u64,
    entries: &[Entry],
) -> Result<Vec<u8>, EncodeError> {
    v2::encode(keys, object_key, batch_id, entries).await
}

/// A size no real version 2 chunk of `entries` exceeds (see [`ENTRY_OVERHEAD_BOUND`] for the key
/// provider assumption). A batcher stops adding entries while this is at most
/// [`MAX_CHUNK_BYTES`]. It never encrypts and never reads a line.
#[must_use]
pub fn encoded_len_upper_bound(entries: &[Entry]) -> u64 {
    encoded_len_upper_bound_of(entries.iter().map(|(_, line)| line.len()))
}

/// [`encoded_len_upper_bound`] from the byte lengths of the lines alone.
#[must_use]
pub fn encoded_len_upper_bound_of(line_lens: impl IntoIterator<Item = usize>) -> u64 {
    let (count, bytes) = line_lens
        .into_iter()
        .fold((0_u64, 0_u64), |(count, bytes), len| {
            (
                count.saturating_add(1),
                bytes.saturating_add(u64::try_from(len).unwrap_or(u64::MAX)),
            )
        });
    encoded_len_upper_bound_for(count, bytes)
}

/// [`encoded_len_upper_bound`] for `count` entries whose lines add up to `line_bytes`: what a
/// batcher asks each time it considers one more entry.
#[must_use]
pub fn encoded_len_upper_bound_for(count: u64, line_bytes: u64) -> u64 {
    CHUNK_FRAMING_BYTES
        .saturating_add(count.saturating_mul(ENTRY_OVERHEAD_BOUND))
        .saturating_add(line_bytes)
}

/// Reads one chunk of the generation `generation` of `tenant`, whichever version it is.
///
/// `keys` is needed for version 2 only; without it a version 2 chunk fails (b).
///
/// # Errors
/// [`ChunkError`] with the A.3 rule that failed.
pub async fn decode(
    tenant: &str,
    generation: u64,
    keys: Option<&TailKeys>,
    object_key: &str,
    bytes: &[u8],
) -> Result<TailChunk, ChunkError> {
    // (a) the key is a canonical tail key of this tenant and generation.
    let batch_id = match Layout::parse(object_key) {
        Ok(Key::Tail(key)) if key.tenant.as_str() == tenant && key.generation == generation => {
            key.batch_id
        }
        _ => {
            return Err(ChunkError::new(
                Rule::A,
                "the key is not a tail key of this tenant and generation",
            ));
        }
    };
    // (b) and (c): the checksum or the MAC first, then the structure.
    if bytes.len() < 32 {
        return Err(ChunkError::new(
            Rule::B,
            "the object is shorter than 32 bytes",
        ));
    }
    let (version, entries) = match bytes.get(..8) {
        Some(magic) if magic == MAGIC_V1.as_slice() => (1, v1::decode(bytes)?),
        Some(magic) if magic == MAGIC_V2.as_slice() => {
            (2, v2::decode(keys, object_key, batch_id, bytes).await?)
        }
        _ => {
            return Err(ChunkError::new(
                Rule::C,
                "the magic is unknown: a reader fails closed",
            ));
        }
    };
    check_entries(&entries, generation)?;
    Ok(TailChunk {
        version,
        batch_id,
        entries,
    })
}

/// A.3 (d) and (e).
fn check_entries(entries: &[Entry], generation: u64) -> Result<(), ChunkError> {
    if entries.is_empty() {
        return Err(ChunkError::new(Rule::D, "the chunk holds no entry"));
    }
    let mut previous: Option<u64> = None;
    for (position, _) in entries {
        if position.generation != generation {
            return Err(ChunkError::new(
                Rule::D,
                "an entry is not in the generation of the key",
            ));
        }
        if previous.is_some_and(|before| before.checked_add(1) != Some(position.seq)) {
            return Err(ChunkError::new(
                Rule::E,
                "an entry does not follow the one before it",
            ));
        }
        previous = Some(position.seq);
    }
    Ok(())
}
