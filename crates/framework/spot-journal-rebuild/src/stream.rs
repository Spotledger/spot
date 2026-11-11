//! Reading the planned segment ranges: the signed manifests as the authority, then one block at
//! a time, in position order, with every ordering and ownership check made on the way.

use std::collections::{BTreeMap, VecDeque};

use spot_events_contract::envelope::Position;
use spot_journal_manifest::{SegmentSummary, SignedManifest, VerifyingKeys, verify};
use spot_journal_rebuild_plan::Step;
use spot_journal_segment::{SegmentReader, segment_hash};
use spot_journal_store::{JournalStore, Layout, StoreError, TenantId};

use crate::entry::{Entry, parse};
use crate::error::RebuildError;

/// The code of a store failure.
pub(crate) fn store_failed(error: &StoreError) -> RebuildError {
    // The store's own stable code, never its message.
    RebuildError::StoreFailed { code: error.code() }
}

/// The verified segment lists of the generations a rebuild reads.
pub(crate) struct Manifests {
    by_generation: BTreeMap<u64, Vec<SegmentSummary>>,
}

impl Manifests {
    /// Reads and verifies the manifest of each generation in `generations`: authentic under
    /// `keys`, naming `source`, and belonging to the generation it was found under.
    pub(crate) async fn load(
        store: &dyn JournalStore,
        keys: &VerifyingKeys,
        tenant: &TenantId,
        source: &str,
        generations: &[u64],
    ) -> Result<Self, RebuildError> {
        let mut by_generation = BTreeMap::new();
        for &generation in generations {
            let invalid =
                |reason: &'static str| RebuildError::ManifestInvalid { generation, reason };
            let key = Layout::manifest(tenant.as_str(), generation)
                .map_err(|e| store_failed(&e))?
                .to_key();
            let object = store
                .get(&key)
                .await
                .map_err(|e| store_failed(&e))?
                .ok_or_else(|| invalid("the manifest is not in the store"))?;
            let signed =
                SignedManifest::from_bytes(&object.bytes).map_err(|e| invalid(e.code()))?;
            let verified = verify(&signed, keys).map_err(|e| invalid(e.code()))?;
            if verified.source() != source || verified.generation() != generation {
                return Err(invalid(
                    "the manifest belongs to another source or generation",
                ));
            }
            by_generation.insert(generation, verified.segments().to_vec());
        }
        Ok(Self { by_generation })
    }

    fn segments(&self, generation: u64) -> &[SegmentSummary] {
        self.by_generation
            .get(&generation)
            .map_or(&[], Vec::as_slice)
    }
}

/// One segment being read.
struct OpenSegment {
    reader: SegmentReader<Vec<u8>>,
    block: usize,
}

/// A stream of parsed entries over the plan's steps, one block per call.
pub(crate) struct Feed<'a> {
    store: &'a dyn JournalStore,
    tenant: &'a TenantId,
    source: &'a str,
    manifests: &'a Manifests,
    steps: &'a [Step],
    step: usize,
    entered: bool,
    next_seq: u64,
    pending: VecDeque<SegmentSummary>,
    open: Option<OpenSegment>,
    peak: usize,
}

impl<'a> Feed<'a> {
    /// A feed over `steps`, whose first entry must be at `first_seq`.
    pub(crate) fn new(
        store: &'a dyn JournalStore,
        tenant: &'a TenantId,
        source: &'a str,
        manifests: &'a Manifests,
        steps: &'a [Step],
        first_seq: u64,
    ) -> Self {
        Self {
            store,
            tenant,
            source,
            manifests,
            steps,
            step: 0,
            entered: false,
            next_seq: first_seq,
            pending: VecDeque::new(),
            open: None,
            peak: 0,
        }
    }

    /// The most entries one call returned.
    pub(crate) fn peak(&self) -> usize {
        self.peak
    }

    /// The next block's entries (restricted to the planned range), or `None` at the end of the
    /// last step.
    pub(crate) async fn next_batch(&mut self) -> Result<Option<Vec<Entry>>, RebuildError> {
        loop {
            let Some(step) = self.steps.get(self.step).copied() else {
                return Ok(None);
            };
            let generation = step.generation();
            if !self.entered {
                if step.first_seq() != self.next_seq {
                    return Err(RebuildError::PositionGap {
                        generation,
                        expected: self.next_seq,
                        found: step.first_seq(),
                    });
                }
                self.pending = self
                    .manifests
                    .segments(generation)
                    .iter()
                    .filter(|s| s.last_seq >= step.first_seq() && s.first_seq <= step.last_seq())
                    .copied()
                    .collect();
                self.entered = true;
            }
            if self.next_seq > step.last_seq() {
                self.step += 1;
                self.entered = false;
                self.open = None;
                self.pending.clear();
                continue;
            }
            if let Some(open) = self.open.as_mut() {
                let Some(index) = open.reader.block_index().get(open.block).copied() else {
                    self.open = None;
                    continue;
                };
                open.block += 1;
                let block_last = index
                    .first_seq
                    .saturating_add(u64::from(index.count).saturating_sub(1));
                let low = index.first_seq.max(self.next_seq);
                let high = block_last.min(step.last_seq());
                if low > high {
                    continue;
                }
                let raw = open.reader.read_range(low, high).map_err(|_| {
                    RebuildError::SegmentInvalid {
                        generation,
                        first_seq: low,
                        last_seq: high,
                        reason: "a block does not read",
                    }
                })?;
                let mut batch = Vec::with_capacity(raw.len());
                for entry in raw {
                    if entry.position.generation != generation
                        || entry.position.seq != self.next_seq
                    {
                        return Err(RebuildError::PositionGap {
                            generation,
                            expected: self.next_seq,
                            found: entry.position.seq,
                        });
                    }
                    batch.push(parse(&entry.line, entry.position, self.source)?);
                    self.next_seq = self.next_seq.saturating_add(1);
                }
                self.peak = self.peak.max(batch.len());
                return Ok(Some(batch));
            }
            let Some(summary) = self.pending.pop_front() else {
                return Err(RebuildError::SegmentMissing {
                    generation,
                    first_seq: self.next_seq,
                    last_seq: step.last_seq(),
                });
            };
            if summary.first_seq > self.next_seq {
                return Err(RebuildError::PositionGap {
                    generation,
                    expected: self.next_seq,
                    found: summary.first_seq,
                });
            }
            self.open = Some(self.open_segment(generation, &summary).await?);
        }
    }

    async fn open_segment(
        &self,
        generation: u64,
        summary: &SegmentSummary,
    ) -> Result<OpenSegment, RebuildError> {
        let invalid = |reason: &'static str| RebuildError::SegmentInvalid {
            generation,
            first_seq: summary.first_seq,
            last_seq: summary.last_seq,
            reason,
        };
        let key = Layout::segment(
            self.tenant.as_str(),
            generation,
            summary.first_seq,
            summary.last_seq,
        )
        .map_err(|e| store_failed(&e))?
        .to_key();
        let object = self
            .store
            .get(&key)
            .await
            .map_err(|e| store_failed(&e))?
            .ok_or(RebuildError::SegmentMissing {
                generation,
                first_seq: summary.first_seq,
                last_seq: summary.last_seq,
            })?;
        if segment_hash(&object.bytes) != summary.segment_hash {
            return Err(invalid("the bytes differ from the manifest's hash"));
        }
        let reader = SegmentReader::open(object.bytes).map_err(|_| invalid("it does not open"))?;
        let header = reader.header();
        if header.source_id != self.source {
            return Err(invalid("the header names another source"));
        }
        if header.generation != generation
            || header.first_seq != summary.first_seq
            || header.last_seq != summary.last_seq
        {
            return Err(invalid("the header disagrees with the manifest"));
        }
        Ok(OpenSegment { reader, block: 0 })
    }
}

/// The position at `seq` in `generation`.
pub(crate) fn position(generation: u64, seq: u64) -> Position {
    Position { generation, seq }
}
