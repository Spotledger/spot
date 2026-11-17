//! The [`JournalReader`]: history from the signed manifest, verified sealed segments and the
//! unsealed tail (ADR-061 s5, Appendix A.6).

use std::sync::Arc;

use spot_events_contract::envelope::{EventEnvelope, Position};
use spot_journal_manifest::{Manifest, SignedManifest, Verified, VerifyingKeys, verify};
use spot_journal_rebuild_plan::unsealed::{TailChunkReader, UnsealedError, read_after};
use spot_journal_segment::{SegmentReader, segment_hash};
use spot_journal_store::{JournalStore, Key, Layout, TenantId};

use crate::error::{HistoryError, unavailable};
use crate::port::{
    DEFAULT_PAGE, HistoryFuture, HistoryPage, JournalHistory, MAX_PAGE, Selector, order,
};

/// Serves history for one tenant from the journal store.
///
/// * A segment is served only after its bytes hash to the manifest's summary, its header agrees
///   with the summary, and every block verifies; otherwise the call fails with
///   `history_unavailable` and nothing of that range is returned.
/// * Positions beyond the manifest come from the tail chunks through
///   [`spot_journal_rebuild_plan::unsealed::read_after`] (A.6), decoded by the injected
///   [`TailChunkReader`] (versions 1 and 2, authenticated and decrypted with the tenant journal
///   key held by the decoder; this crate never sees key material).
/// * A generation a fork abandoned serves its valid part (up to the fork point) and then
///   continues in the next generation; a position after the fork point is `resync_required`.
/// * Personal fields are returned exactly as stored, still sealed. The stream never reveals them:
///   a consumer sees the envelope or the erased placeholder, and a `Reveal` seam is not offered
///   here (the default and only behaviour is never to reveal).
///
/// Memory: one segment object is held at a time and a page scans at most [`MAX_PAGE`] entries.
pub struct JournalReader<S: JournalStore> {
    store: Arc<S>,
    tenant: String,
    keys: Arc<VerifyingKeys>,
    chunks: Arc<dyn TailChunkReader>,
}

impl<S: JournalStore> JournalReader<S> {
    /// A reader for `tenant`.
    ///
    /// # Errors
    /// [`HistoryError::ForeignKey`] when the tenant id is not one safe path component (`../x`).
    pub fn new(
        store: Arc<S>,
        tenant: &str,
        keys: Arc<VerifyingKeys>,
        chunks: Arc<dyn TailChunkReader>,
    ) -> Result<Self, HistoryError> {
        TenantId::new(tenant).map_err(|_| HistoryError::ForeignKey)?;
        Ok(Self {
            store,
            tenant: tenant.to_owned(),
            keys,
            chunks,
        })
    }

    /// A key of this tenant, checked to be canonical and ours.
    fn own_key(
        &self,
        key: Result<Key, spot_journal_store::StoreError>,
    ) -> Result<String, HistoryError> {
        let key = key.map_err(|_| HistoryError::ForeignKey)?;
        if key.tenant().as_str() != self.tenant {
            return Err(HistoryError::ForeignKey);
        }
        let text = key.to_key();
        match Layout::parse(&text) {
            Ok(parsed) if parsed == key => Ok(text),
            _ => Err(HistoryError::ForeignKey),
        }
    }

    async fn manifest(&self, generation: u64) -> Result<Option<Verified>, HistoryError> {
        let key = self.own_key(Layout::manifest(&self.tenant, generation))?;
        let Some(stored) = self
            .store
            .get(&key)
            .await
            .map_err(|_| unavailable("the store failed reading a manifest"))?
        else {
            return Ok(None);
        };
        let signed = SignedManifest::from_bytes(&stored.bytes)
            .map_err(|_| unavailable("a manifest is not canonical"))?;
        let verified =
            verify(&signed, &self.keys).map_err(|_| unavailable("a manifest does not verify"))?;
        if verified.generation() != generation {
            return Err(unavailable("a manifest is stored under another generation"));
        }
        Ok(Some(verified))
    }

    /// The segment bytes, verified against the manifest summary before anything is served.
    async fn segment_bytes(
        &self,
        manifest: &Manifest,
        index: usize,
    ) -> Result<Vec<u8>, HistoryError> {
        let summary = manifest
            .segments
            .get(index)
            .ok_or_else(|| unavailable("a segment index is outside the manifest"))?;
        if summary.generation != manifest.generation {
            return Err(HistoryError::ForeignKey);
        }
        let key = self.own_key(Layout::segment(
            &self.tenant,
            summary.generation,
            summary.first_seq,
            summary.last_seq,
        ))?;
        let stored = self
            .store
            .get(&key)
            .await
            .map_err(|_| unavailable("the store failed reading a segment"))?
            .ok_or_else(|| unavailable("a segment the manifest lists is missing"))?;
        if segment_hash(&stored.bytes) != summary.segment_hash {
            return Err(unavailable(
                "a segment does not hash to its manifest summary",
            ));
        }
        let reader = SegmentReader::open(stored.bytes.as_slice())
            .map_err(|_| unavailable("a segment does not open"))?;
        let header = reader.header();
        if header.source_id != manifest.source {
            return Err(HistoryError::ForeignKey);
        }
        if header.generation != summary.generation
            || header.first_seq != summary.first_seq
            || header.last_seq != summary.last_seq
            || header.entry_count != summary.entry_count
            || header.prev_hash != summary.previous_hash
        {
            return Err(unavailable("a segment header disagrees with its summary"));
        }
        reader
            .verify_all()
            .map_err(|_| unavailable("a segment block does not verify"))?;
        Ok(stored.bytes)
    }

    fn parse(line: &[u8]) -> Result<EventEnvelope, HistoryError> {
        let text = std::str::from_utf8(line).map_err(|_| unavailable("an entry is not UTF-8"))?;
        EventEnvelope::from_json(text).map_err(|_| unavailable("an entry is not an envelope"))
    }
}

/// The accumulating page.
struct Scan<'a> {
    max: usize,
    through: Option<Position>,
    select: &'a Selector,
    envelopes: Vec<EventEnvelope>,
    scanned: usize,
    last: Position,
    /// The page stopped because it is full.
    full: bool,
    /// The page stopped because the `through` bound was passed.
    bounded: bool,
}

impl Scan<'_> {
    /// Offers the entry at `position`; `false` once the scan must stop.
    fn offer(&mut self, position: Position, envelope: &EventEnvelope) -> bool {
        if let Some(through) = self.through
            && order(position) > order(through)
        {
            self.bounded = true;
            return false;
        }
        if self.scanned >= self.max {
            self.full = true;
            return false;
        }
        self.scanned += 1;
        self.last = position;
        if (self.select)(envelope) {
            self.envelopes.push(envelope.clone());
        }
        true
    }
}

impl<S: JournalStore> JournalReader<S> {
    async fn read(
        &self,
        after: Position,
        max: usize,
        through: Option<Position>,
        select: &Selector,
    ) -> Result<HistoryPage, HistoryError> {
        if after.generation == 0 && after.seq != 0 {
            return Err(HistoryError::RequestInvalid(
                "generation 0 is only the start position",
            ));
        }
        let max = match max {
            0 => DEFAULT_PAGE,
            n => n.min(MAX_PAGE),
        };
        let mut scan = Scan {
            max,
            through,
            select,
            envelopes: Vec::new(),
            scanned: 0,
            last: after,
            full: false,
            bounded: false,
        };
        let mut generation = after.generation.max(1);
        let mut after_seq = if after.generation == 0 { 0 } else { after.seq };
        let mut exhausted = false;

        loop {
            let Some(current) = self.manifest(generation).await? else {
                if after.generation == 0 && generation == 1 {
                    // A journal that holds nothing yet.
                    exhausted = true;
                    break;
                }
                return Err(unavailable("the manifest of a generation is missing"));
            };
            let manifest = current.manifest();
            let successor = self.manifest(generation.saturating_add(1)).await?;
            let mut valid_end = u64::MAX;
            if let Some(next) = &successor {
                let fork = next
                    .manifest()
                    .forks
                    .iter()
                    .find(|fork| fork.generation == generation.saturating_add(1))
                    .ok_or_else(|| unavailable("a successor manifest has no fork record"))?;
                if next.manifest().previous_manifest_hash != Some(manifest.manifest_hash()) {
                    return Err(unavailable(
                        "the manifest chain between generations is broken",
                    ));
                }
                valid_end = fork.forked_from_position;
                if generation == after.generation && after.seq > valid_end {
                    return Err(HistoryError::ResyncRequired {
                        generation: after.generation,
                        seq: after.seq,
                    });
                }
            }

            // Sealed history.
            for (index, summary) in manifest.segments.iter().enumerate() {
                if summary.last_seq <= after_seq || summary.first_seq > valid_end {
                    continue;
                }
                // Do not fetch a segment this page cannot use.
                if scan.scanned >= scan.max {
                    scan.full = true;
                    break;
                }
                if let Some(bound) = through
                    && order(Position {
                        generation,
                        seq: summary.first_seq,
                    }) > order(bound)
                {
                    scan.bounded = true;
                    break;
                }
                let bytes = self.segment_bytes(manifest, index).await?;
                let reader = SegmentReader::open(bytes.as_slice())
                    .map_err(|_| unavailable("a segment does not open"))?;
                for entry in reader.entries() {
                    let entry =
                        entry.map_err(|_| unavailable("a segment block does not verify"))?;
                    if entry.position.seq <= after_seq {
                        continue;
                    }
                    if entry.position.seq > valid_end {
                        break;
                    }
                    let envelope = Self::parse(entry.line.as_bytes())?;
                    if !scan.offer(entry.position, &envelope) {
                        break;
                    }
                }
                if scan.full || scan.bounded {
                    break;
                }
            }

            // Unsealed tail beyond the manifest (A.6).
            if !scan.full && !scan.bounded {
                let tail = read_after(
                    self.store.as_ref(),
                    &self.tenant,
                    generation,
                    manifest.last_seq(),
                    self.chunks.as_ref(),
                )
                .await
                .map_err(|error| match error {
                    UnsealedError::Damaged { .. } => unavailable("a tail chunk is damaged"),
                    UnsealedError::Gap { .. } => unavailable("the tail chunks have a gap"),
                    UnsealedError::Tampered { .. } => unavailable("a tail chunk was tampered with"),
                    UnsealedError::Store(_) => unavailable("the store failed reading the tail"),
                })?;
                for entry in tail.entries() {
                    let entry = entry.get();
                    if entry.position.seq <= after_seq {
                        continue;
                    }
                    if entry.position.seq > valid_end {
                        break;
                    }
                    let envelope = Self::parse(&entry.line)?;
                    let position = Position {
                        generation: entry.position.generation,
                        seq: entry.position.seq,
                    };
                    if !scan.offer(position, &envelope) {
                        break;
                    }
                }
            }
            if scan.full || scan.bounded {
                break;
            }
            if successor.is_some() {
                generation = generation.saturating_add(1);
                after_seq = 0;
                continue;
            }
            exhausted = true;
            break;
        }

        Ok(HistoryPage {
            envelopes: scan.envelopes,
            next_after: scan.last,
            reached_outbox: exhausted || scan.bounded,
        })
    }
}

impl<S: JournalStore + 'static> JournalHistory for JournalReader<S> {
    fn read_after<'a>(
        &'a self,
        after: Position,
        max: usize,
        through: Option<Position>,
        select: &'a Selector,
    ) -> HistoryFuture<'a, HistoryPage> {
        Box::pin(self.read(after, max, through, select))
    }

    fn first_available(&self) -> HistoryFuture<'_, Option<Position>> {
        Box::pin(async move {
            let selector: &Selector = &|_| true;
            let start = Position {
                generation: 0,
                seq: 0,
            };
            let page = self.read(start, 1, None, selector).await?;
            Ok((page.next_after.generation != 0).then_some(page.next_after))
        })
    }
}
