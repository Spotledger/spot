//! The reader for unsealed positions (WP-JRN-05.T8, ADR-061 s5 and Appendix A.3, A.4, A.6).
//!
//! Under `remote-sync` a commit is acknowledged once its tail chunk is durable, which can be
//! before the chunk is sealed into a segment and listed in the signed manifest. Those positions
//! exist **only** in the chunks. [`read_after`] is the reader that makes "readers see one stream"
//! true: it lists the chunks of one generation, applies the rules of A.6 and returns the entries
//! beyond the manifest in stream order, marked [`Unsealed`].
//!
//! # What it does
//!
//! 1. Lists `<tenant>/tail/<generation>/` and orders the chunks by `batch_id` (never by the
//!    lexical order of the listing, A.4).
//! 2. Decodes each chunk through a [`TailChunkReader`] (the relay's version 1 and version 2
//!    decoder is plugged in by the caller, so this crate neither holds key material nor knows a
//!    chunk layout) and checks A.3 (d) and (e) itself.
//! 3. Classifies the chunk against `manifest_last_seq`:
//!    * **covered** (`last_seq <= manifest_last_seq`): reported in [`UnsealedTail::covered`]; the
//!      caller may delete it (A.5), this function never does;
//!    * **straddling** (starts at or before, ends after): [`UnsealedError::Tampered`]; A.5 cannot
//!      produce one, so someone edited the object and it is never merged;
//!    * **beyond**: the only copy of those positions; A.3 (f) is required (the first starts at
//!      `manifest_last_seq + 1`, each next one at the previous last `seq` plus one), otherwise
//!      [`UnsealedError::Gap`].
//! 4. A chunk that fails A.3 (a) to (e) stops the generation with the letter that failed
//!    ([`UnsealedError::Damaged`]); it is never skipped, repaired or deleted (an acknowledgement
//!    was already returned, so skipping would lose an acknowledged commit).
//!
//! # What it never does
//!
//! It writes and deletes nothing: the only store calls are `list`, `get` and `head`. The entries
//! come back wrapped in [`Unsealed`], which has no constructor outside this module and no
//! conversion to a hash-chain link, so a caller cannot present them as sealed history or sign
//! them. Integration with `plan()` is a later step; this function is exported for the planner.
//!
//! Time, randomness and key material are not read here: keys are held by the
//! [`TailChunkReader`] the caller passes.

use spot_journal_store::{JournalStore, Key, Layout, StoreError, TAIL_PREFIX, TailKey, TenantId};

/// Objects per listing page.
const PAGE: usize = 1000;

/// A position in the stream: `(generation, seq)`, ordered lexicographically (ADR-061 A.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TailPosition {
    /// The generation.
    pub generation: u64,
    /// The sequence number inside the generation; restarts at 1 in every generation.
    pub seq: u64,
}

/// One entry of a decoded chunk: its position and its envelope line (opaque bytes; never shown
/// in an error or a log).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailEntry {
    /// The position the entry claims.
    pub position: TailPosition,
    /// The envelope line.
    pub line: Vec<u8>,
}

/// Why a chunk did not decode: the letter of the A.3 rule that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkFault {
    /// `'b'` (size or checksum or MAC), `'c'` (magic or length), `'d'` (generation or count) or
    /// `'e'` (entry order inside the chunk). Other letters are reported as `'c'`.
    pub letter: char,
}

/// A decoder for tail chunks of every version still present (A.7). The relay supplies the real
/// one (version 1 and 2); a test supplies its own.
pub trait TailChunkReader: Send + Sync {
    /// Decodes the chunk stored under `key`, verifying its authentication **before** it parses or
    /// decrypts anything (A.2.2). The object key is an argument because version 2 covers it in
    /// the MAC.
    ///
    /// # Errors
    /// The A.3 letter that failed.
    fn decode(
        &self,
        key: &TailKey,
        object_key: &str,
        bytes: &[u8],
    ) -> Result<Vec<TailEntry>, ChunkFault>;
}

/// A value read from a position the manifest does not cover. It has no public constructor and no
/// conversion into a sealed record, so it cannot be mistaken for sealed history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsealed<T> {
    value: T,
}

impl<T> Unsealed<T> {
    fn new(value: T) -> Self {
        Self { value }
    }

    /// The value, still marked unsealed by the type that carried it.
    #[must_use]
    pub fn get(&self) -> &T {
        &self.value
    }

    /// The value, consuming the marker. The caller states that it knows the value is unsealed.
    #[must_use]
    pub fn into_unsealed_value(self) -> T {
        self.value
    }
}

/// A chunk the manifest already covers: the caller may delete it (A.5); this reader did not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoveredChunk {
    /// The chunk's batch id.
    pub batch_id: u64,
    /// The canonical object key.
    pub object_key: String,
}

/// The unsealed positions of one generation, in `(generation, seq)` order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsealedTail {
    generation: u64,
    manifest_last_seq: u64,
    entries: Vec<Unsealed<TailEntry>>,
    covered: Vec<CoveredChunk>,
    closed: bool,
}

impl UnsealedTail {
    /// The generation read.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The last `seq` the manifest covered when the tail was read.
    #[must_use]
    pub fn manifest_last_seq(&self) -> u64 {
        self.manifest_last_seq
    }

    /// The entries beyond the manifest, ascending by `seq`, each marked [`Unsealed`].
    #[must_use]
    pub fn entries(&self) -> &[Unsealed<TailEntry>] {
        &self.entries
    }

    /// The chunks the manifest already covers (candidates for A.5 deletion by the caller).
    #[must_use]
    pub fn covered(&self) -> &[CoveredChunk] {
        &self.covered
    }

    /// Whether the generation is closed: its successor's manifest exists, so it takes no new
    /// chunks (A.4). The chunks present at closing time are still read.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The last position the tail holds, or `None` when nothing lies beyond the manifest.
    #[must_use]
    pub fn last_position(&self) -> Option<TailPosition> {
        self.entries.last().map(|entry| entry.value.position)
    }

    /// Whether nothing lies beyond the manifest.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Why the unsealed tail could not be read. Every variant stops the generation: nothing is
/// skipped, repaired or deleted. No variant carries an entry line.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UnsealedError {
    /// A chunk failed A.3 (a) to (e); the letter says which rule.
    #[error("tail chunk {batch_id} is damaged: rule A.3 ({letter}) failed")]
    Damaged {
        /// The chunk's batch id (`0` when the key itself is not canonical).
        batch_id: u64,
        /// The A.3 letter.
        letter: char,
    },
    /// A beyond-the-manifest chunk does not continue the previous one (A.3 (f)).
    #[error("the tail has a gap: expected seq {expected} but chunk {batch_id} starts at {found}")]
    Gap {
        /// The seq the chain required next.
        expected: u64,
        /// The first seq the chunk holds.
        found: u64,
        /// The chunk's batch id.
        batch_id: u64,
    },
    /// A chunk straddles the manifest, or is out of order against the chunks before it: someone
    /// edited the objects (A.5 cannot produce either). Never merged.
    #[error("tail chunk {batch_id} straddles the manifest or breaks the batch order: tampering")]
    Tampered {
        /// The chunk's batch id.
        batch_id: u64,
    },
    /// The object store failed or refused.
    #[error("the journal store failed: {0}")]
    Store(StoreError),
}

impl UnsealedError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Damaged { .. } => "journal_rebuild_plan.unsealed_damaged",
            Self::Gap { .. } => "journal_rebuild_plan.unsealed_gap",
            Self::Tampered { .. } => "journal_rebuild_plan.unsealed_tampered",
            Self::Store(_) => "journal_rebuild_plan.unsealed_store_failed",
        }
    }
}

impl From<StoreError> for UnsealedError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

fn damaged(batch_id: u64, letter: char) -> UnsealedError {
    UnsealedError::Damaged { batch_id, letter }
}

/// Checks A.3 (d) and (e) on a decoded chunk, whatever the decoder promised.
fn check_entries(
    batch_id: u64,
    generation: u64,
    entries: &[TailEntry],
) -> Result<(u64, u64), UnsealedError> {
    let (Some(first), Some(last)) = (entries.first(), entries.last()) else {
        return Err(damaged(batch_id, 'd'));
    };
    if entries
        .iter()
        .any(|entry| entry.position.generation != generation)
    {
        return Err(damaged(batch_id, 'd'));
    }
    let consecutive = entries.windows(2).all(|pair| {
        pair.first()
            .zip(pair.get(1))
            .is_some_and(|(a, b)| a.position.seq.checked_add(1) == Some(b.position.seq))
    });
    if !consecutive {
        return Err(damaged(batch_id, 'e'));
    }
    Ok((first.position.seq, last.position.seq))
}

async fn list_chunks<S: JournalStore + ?Sized>(
    store: &S,
    tenant: &str,
    generation: u64,
) -> Result<Vec<(TailKey, String)>, UnsealedError> {
    let prefix = format!("{tenant}/{TAIL_PREFIX}/{}/", Layout::padded(generation));
    let mut keys: Vec<String> = Vec::new();
    loop {
        let after = keys.last().cloned();
        let page = store.list(&prefix, after.as_deref(), PAGE).await?;
        let full = page.len() >= PAGE;
        keys.extend(page.into_iter().map(|entry| entry.key));
        if !full {
            break;
        }
    }
    let mut chunks = Vec::with_capacity(keys.len());
    for object_key in keys {
        match Layout::parse(&object_key) {
            // A.3 (a): the key must be a canonical tail key of the generation read.
            Ok(Key::Tail(tail)) if tail.generation == generation => {
                chunks.push((tail, object_key));
            }
            _ => return Err(damaged(0, 'a')),
        }
    }
    // A.4: ascending batch id, never the lexical order of the listing.
    chunks.sort_by_key(|(tail, _)| tail.batch_id);
    Ok(chunks)
}

/// Reads the unsealed positions of `generation`: everything the chunks hold beyond
/// `manifest_last_seq`, in stream order, under the rules of A.6.
///
/// `chunks` decodes each chunk (version 1 and 2). The function lists, reads and checks; it never
/// writes or deletes (covered chunks are reported, not removed).
///
/// # Errors
/// [`UnsealedError::Damaged`] with the A.3 letter, [`UnsealedError::Gap`],
/// [`UnsealedError::Tampered`] for a straddling or out-of-order chunk,
/// [`UnsealedError::Store`] when the store fails.
pub async fn read_after<S, R>(
    store: &S,
    tenant: &str,
    generation: u64,
    manifest_last_seq: u64,
    chunks: &R,
) -> Result<UnsealedTail, UnsealedError>
where
    S: JournalStore + ?Sized,
    R: TailChunkReader + ?Sized,
{
    let _tenant = TenantId::new(tenant)?;
    let listed = list_chunks(store, tenant, generation).await?;
    let mut entries: Vec<Unsealed<TailEntry>> = Vec::new();
    let mut covered = Vec::new();
    // The seq the next beyond-the-manifest chunk must start at (A.3 (f)).
    let mut expected = manifest_last_seq
        .checked_add(1)
        .ok_or_else(|| damaged(0, 'e'))?;
    let mut seen_beyond = false;

    for (tail, object_key) in listed {
        let batch_id = tail.batch_id;
        let Some(object) = store.get(&object_key).await? else {
            // Compacted and deleted between the listing and the read: A.5 removed it after a
            // manifest covered it, so it holds nothing this reader needs. A chunk that vanished
            // for any other reason shows up as a gap in the chain below.
            continue;
        };
        let decoded = chunks
            .decode(&tail, &object_key, &object.bytes)
            .map_err(|fault| damaged(batch_id, fault.letter))?;
        let (first_seq, last_seq) = check_entries(batch_id, generation, &decoded)?;

        if last_seq <= manifest_last_seq {
            // A covered chunk after a beyond chunk breaks the batch order (A.4).
            if seen_beyond {
                return Err(UnsealedError::Tampered { batch_id });
            }
            covered.push(CoveredChunk {
                batch_id,
                object_key,
            });
            continue;
        }
        if first_seq <= manifest_last_seq {
            return Err(UnsealedError::Tampered { batch_id });
        }
        if first_seq != expected {
            return Err(UnsealedError::Gap {
                expected,
                found: first_seq,
                batch_id,
            });
        }
        seen_beyond = true;
        expected = last_seq
            .checked_add(1)
            .ok_or_else(|| damaged(batch_id, 'e'))?;
        entries.extend(decoded.into_iter().map(Unsealed::new));
    }

    let successor = Layout::manifest(tenant, generation.saturating_add(1))?.to_key();
    let closed = store.head(&successor).await?.is_some();
    Ok(UnsealedTail {
        generation,
        manifest_last_seq,
        entries,
        covered,
        closed,
    })
}
