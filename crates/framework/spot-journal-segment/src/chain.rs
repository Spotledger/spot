//! The chain verifier: each header commits to the hash of the previous segment.

use crate::error::ChainError;
use crate::format::{Header, genesis_hash, segment_hash};
use crate::reader::SegmentReader;
use spot_events_contract::envelope::Position;

/// Segment bytes as the verifier takes them (any `AsRef<[u8]>` works).
pub type SegmentBytes = Vec<u8>;

/// What a verified chain covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainReport {
    /// Number of segments verified.
    pub segments: usize,
    /// Number of entries across them.
    pub entries: u64,
    /// First position covered, if any entry exists.
    pub first: Option<Position>,
    /// Last position covered, if any entry exists.
    pub last: Option<Position>,
    /// Hash of the last segment: what the next segment must carry as `prev_hash`.
    pub head_hash: [u8; 32],
}

/// Verify a chain that starts at the genesis of its source.
///
/// Every segment is opened and all its blocks are checked, so a damaged last segment (which no
/// later header vouches for) is found too. Then, for each segment after the first, the
/// previous-hash link and the position continuity are checked.
///
/// # Errors
/// [`ChainError`] naming the first bad segment.
pub fn verify_chain<B: AsRef<[u8]>>(segments: &[B]) -> Result<ChainReport, ChainError> {
    verify_chain_from(None, segments)
}

/// Like [`verify_chain`], for a chain that continues a known segment: `anchor` is that
/// segment's hash (and the first segment must carry it); `None` means the genesis of the
/// first segment's source.
///
/// # Errors
/// [`ChainError`] naming the first bad segment.
pub fn verify_chain_from<B: AsRef<[u8]>>(
    anchor: Option<[u8; 32]>,
    segments: &[B],
) -> Result<ChainReport, ChainError> {
    let mut headers: Vec<Header> = Vec::with_capacity(segments.len());
    let mut hashes: Vec<[u8; 32]> = Vec::with_capacity(segments.len());
    for (index, seg) in segments.iter().enumerate() {
        let bytes = seg.as_ref();
        let altered = |e: crate::SegmentError| ChainError::Altered {
            index,
            reason: e.to_string(),
        };
        let reader = SegmentReader::open(bytes).map_err(altered)?;
        reader.verify_all().map_err(altered)?;
        headers.push(reader.header().clone());
        hashes.push(segment_hash(bytes));
    }

    let mut entries = 0u64;
    let mut first: Option<Position> = None;
    let mut last: Option<Position> = None;
    let mut head = anchor.unwrap_or([0; 32]);
    for (i, h) in headers.iter().enumerate() {
        if i == 0 {
            let want = anchor.unwrap_or_else(|| genesis_hash(&h.source_id));
            if h.prev_hash != want {
                return Err(ChainError::Missing { index: 0 });
            }
        } else {
            let prev = &headers[i - 1];
            if h.source_id != prev.source_id {
                return Err(ChainError::SourceMismatch { index: i });
            }
            if h.prev_hash != hashes[i - 1] {
                return Err(classify_break(i, &headers, &hashes, last));
            }
        }
        // Linked correctly: now the positions.
        if h.entry_count > 0 {
            if let Some(l) = last {
                if h.generation < l.generation {
                    return Err(ChainError::GenerationMismatch { index: i });
                }
                if h.generation == l.generation && h.first_seq != l.seq.saturating_add(1) {
                    return Err(ChainError::PositionGap { index: i });
                }
            }
            first.get_or_insert(Position {
                generation: h.generation,
                seq: h.first_seq,
            });
            last = Some(Position {
                generation: h.generation,
                seq: h.last_seq,
            });
            entries += h.entry_count;
        }
        head = hashes[i];
    }
    Ok(ChainReport {
        segments: segments.len(),
        entries,
        first,
        last,
        head_hash: head,
    })
}

/// The previous-hash link of segment `i` does not match the segment before it: say why.
fn classify_break(
    i: usize,
    headers: &[Header],
    hashes: &[[u8; 32]],
    last: Option<Position>,
) -> ChainError {
    let h = &headers[i];
    if hashes
        .iter()
        .enumerate()
        .any(|(j, x)| j > i - 1 && *x == h.prev_hash && j != i)
    {
        // Its predecessor is further down the list: the list is out of order.
        return ChainError::Reordered { index: i };
    }
    if let (Some(l), true) = (last, h.entry_count > 0) {
        if h.generation < l.generation {
            return ChainError::GenerationMismatch { index: i };
        }
        if h.generation == l.generation {
            if h.first_seq <= l.seq {
                return ChainError::PositionGap { index: i };
            }
            if h.first_seq > l.seq.saturating_add(1) {
                return ChainError::Missing { index: i };
            }
        } else {
            return ChainError::Missing { index: i };
        }
    }
    // Adjacent in position yet chained to other bytes: the previous segment is not the one
    // this header was sealed after.
    ChainError::Altered {
        index: i - 1,
        reason: "the next segment commits to different bytes".to_owned(),
    }
}
