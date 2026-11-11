//! Creating the generation's manifest and repairing a crash between any two sealing steps.

use spot_events_contract::envelope::Position;
use spot_journal_manifest::{Manifest, SegmentSummary, SignedManifest, verify};
use spot_journal_segment::{SegmentReader, genesis_hash, segment_hash};
use spot_journal_store::{
    JournalStore, Key, Layout, ObjectEntry, SEGMENTS_PREFIX, StoreError, TAIL_PREFIX,
};

use crate::chunk::{self, Entry};
use crate::config::Durability;
use crate::error::RelayError;
use crate::relay::{Relay, hex};

const PAGE: usize = 1000;

fn broken(reason: &'static str) -> RelayError {
    RelayError::ChainBroken { reason }
}

/// A tail chunk read back from the store.
struct Chunk {
    batch_id: u64,
    entries: Vec<Entry>,
}

impl Chunk {
    fn first_seq(&self) -> u64 {
        self.entries.first().map_or(0, |(position, _)| position.seq)
    }
    fn last_seq(&self) -> u64 {
        self.entries.last().map_or(0, |(position, _)| position.seq)
    }
}

impl<S: JournalStore> Relay<S> {
    /// Writes the generation's first signed manifest unless the store already has one.
    pub(crate) async fn create_manifest_if_absent(
        &mut self,
        now_millis: i64,
    ) -> Result<(), RelayError> {
        let key = self.manifest_key()?;
        if self.store.head(&key).await?.is_some() {
            return Ok(());
        }
        // A missing manifest next to segment objects of this generation is a broken chain, not a
        // fresh start: signing a new manifest beside segments it does not list would look
        // authoritative and hide history. Refuse on the mere presence of any object (a false
        // refusal costs an operator action; a false creation costs history). Nothing is written.
        // A tail chunk alone is fine: `resume` adopts it after the first manifest exists.
        let segment_prefix = format!(
            "{}/{}/{}/",
            self.cfg.tenant,
            SEGMENTS_PREFIX,
            Layout::padded(self.generation)
        );
        if !self.list_all(&segment_prefix).await?.is_empty() {
            return Err(broken("segments exist but the manifest is missing"));
        }
        let manifest = match &self.fork {
            None => Manifest::first(&self.cfg.source, now_millis)
                .map_err(|_| broken("the first manifest could not be built"))?,
            Some(fork) => {
                let previous_key =
                    Layout::manifest(&self.cfg.tenant, fork.generation - 1)?.to_key();
                let stored = self
                    .store
                    .get(&previous_key)
                    .await?
                    .ok_or_else(|| broken("the abandoned generation's manifest is missing"))?;
                let previous = self.verified_manifest(&stored.bytes)?;
                if previous.source != self.cfg.source {
                    return Err(broken("the abandoned generation belongs to another source"));
                }
                let forked = previous
                    .fork(
                        fork.forked_from_position,
                        &fork.reason,
                        fork.at_millis,
                        now_millis,
                    )
                    .map_err(|_| broken("the abandoned generation cannot be forked there"))?;
                if forked.generation != fork.generation {
                    return Err(broken("the fork record opens another generation"));
                }
                forked
            }
        };
        let signed = self.signer.sign(&manifest);
        match self.store.put_new(&key, signed.to_bytes()).await {
            Ok(_) => Ok(()),
            Err(StoreError::AlreadyExists { .. }) => {
                Err(self.halt("another writer created the manifest first"))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn verified_manifest(&self, bytes: &[u8]) -> Result<Manifest, RelayError> {
        let signed = SignedManifest::from_bytes(bytes)
            .map_err(|_| broken("the stored manifest is not a canonical signed manifest"))?;
        let verified = verify(&signed, &self.keys)
            .map_err(|_| broken("the stored manifest does not verify"))?;
        Ok(verified.manifest().clone())
    }

    async fn list_all(&self, prefix: &str) -> Result<Vec<ObjectEntry>, RelayError> {
        let mut out: Vec<ObjectEntry> = Vec::new();
        loop {
            let after = out.last().map(|entry| entry.key.clone());
            let page = self.store.list(prefix, after.as_deref(), PAGE).await?;
            let full = page.len() >= PAGE;
            out.extend(page);
            if !full {
                return Ok(out);
            }
        }
    }

    /// Rebuilds the relay's view from the store and repairs what a crash left half done.
    ///
    /// In order: read and verify the manifest; compare it with the segment listing (ranges for
    /// every segment, the stored bytes of the last one); adopt segment objects the manifest lacks,
    /// verifying header range, hash chain and blocks first, with one guarded manifest replace;
    /// delete tail chunks the manifest covers; and seal the tail chunks that remain into a
    /// segment. Under `local` and `replica-sync` the entries the relay still holds in memory
    /// beyond the manifest are kept; entries the process lost are replayed by the caller from
    /// [`Relay::next_position`].
    ///
    /// # Errors
    /// `chain_broken` when the stored chain disagrees with the manifest (nothing is overwritten),
    /// `split_brain` when the guarded manifest replace loses, `store_failed`.
    pub async fn resume(&mut self) -> Result<(), RelayError> {
        if self.halted {
            return Err(self.halt("the relay stopped after it found another writer"));
        }
        self.needs_resume = true;

        // 1. The manifest.
        let manifest_key = self.manifest_key()?;
        let stored = self
            .store
            .get(&manifest_key)
            .await?
            .ok_or_else(|| broken("the manifest is missing"))?;
        let mut manifest = self.verified_manifest(&stored.bytes)?;
        if manifest.source != self.cfg.source || manifest.generation != self.generation {
            return Err(broken(
                "the manifest belongs to another source or generation",
            ));
        }
        if let Some(fork) = &self.fork
            && manifest.forks.last() != Some(fork)
        {
            return Err(broken("the manifest records a different fork"));
        }
        let newer = Layout::manifest(&self.cfg.tenant, self.generation.saturating_add(1))?;
        if self.store.head(&newer.to_key()).await?.is_some() {
            return Err(broken(
                "a newer generation exists: this generation is closed",
            ));
        }
        let mut etag = stored.meta.etag;

        // 2. Segments the store holds, against the manifest.
        let prefix = format!(
            "{}/{}/{}/",
            self.cfg.tenant,
            SEGMENTS_PREFIX,
            Layout::padded(self.generation)
        );
        let mut stored_segments = Vec::new();
        for entry in self.list_all(&prefix).await? {
            match Layout::parse(&entry.key) {
                Ok(Key::Segment(key)) => stored_segments.push((key.first_seq, key.last_seq)),
                _ => {
                    return Err(broken(
                        "an object in the segment listing is not a segment key",
                    ));
                }
            }
        }
        stored_segments.sort_unstable();
        // Every segment the manifest lists must be stored. Segments it does not list are either
        // strays below its end (a loser's orphan; segments are never deleted, and a stray does not
        // make the manifest untrue) or continue it and are adopted below.
        if manifest.segments.iter().any(|s| {
            stored_segments
                .binary_search(&(s.first_seq, s.last_seq))
                .is_err()
        }) {
            return Err(broken("the stored segments disagree with the manifest"));
        }
        let covered = manifest.last_seq();
        let beyond: Vec<(u64, u64)> = stored_segments
            .iter()
            .copied()
            .filter(|(_, last_seq)| *last_seq > covered)
            .collect();
        if let Some(last) = manifest.segments.last() {
            let key = Layout::segment(
                &self.cfg.tenant,
                self.generation,
                last.first_seq,
                last.last_seq,
            )?
            .to_key();
            let meta = self.store.head(&key).await?;
            if meta.is_none_or(|meta| meta.etag != hex(&last.segment_hash)) {
                return Err(broken("the last stored segment differs from the manifest"));
            }
        }

        // 3. Adopt segments the manifest lacks.
        let mut head_hash = manifest
            .segments
            .last()
            .map_or_else(|| genesis_hash(&self.cfg.source), |s| s.segment_hash);
        let mut expected = manifest.next_seq();
        let mut adopted: Vec<SegmentSummary> = Vec::new();
        for (first_seq, last_seq) in beyond {
            if first_seq != expected {
                return Err(broken("a stored segment does not continue the manifest"));
            }
            let key =
                Layout::segment(&self.cfg.tenant, self.generation, first_seq, last_seq)?.to_key();
            let object = self
                .store
                .get(&key)
                .await?
                .ok_or_else(|| broken("a listed segment vanished"))?;
            let reader = SegmentReader::open(object.bytes.as_slice())
                .map_err(|_| broken("a stored segment is damaged"))?;
            reader
                .verify_all()
                .map_err(|_| broken("a stored segment is damaged"))?;
            let header = reader.header();
            let fits = header.source_id == self.cfg.source
                && header.generation == self.generation
                && header.first_seq == first_seq
                && header.last_seq == last_seq
                && header.entry_count == last_seq - first_seq + 1
                && header.prev_hash == head_hash;
            if !fits {
                return Err(broken("a stored segment does not chain to the manifest"));
            }
            let hash = segment_hash(&object.bytes);
            adopted.push(SegmentSummary::new(
                self.generation,
                first_seq,
                last_seq,
                header.entry_count,
                hash,
                head_hash,
            ));
            head_hash = hash;
            expected = last_seq + 1;
        }
        if !adopted.is_empty() {
            let mut next = manifest
                .append_segments(&adopted)
                .map_err(|_| broken("the stored segments do not extend the manifest"))?;
            next.issued_at_millis = self.last_now;
            let signed = self.signer.sign(&next);
            match self
                .store
                .put_replace(&manifest_key, signed.to_bytes(), &etag)
                .await
            {
                Ok(meta) => {
                    manifest = next;
                    etag = meta.etag;
                }
                Err(StoreError::EtagMismatch { .. }) => {
                    return Err(self.halt("the manifest replace lost to another writer"));
                }
                Err(error) => return Err(error.into()),
            }
        }

        // 4. Tail chunks: delete what the manifest covers, keep the rest in order.
        let covered_to = manifest.last_seq();
        let mut remaining: Vec<Chunk> = Vec::new();
        for chunk in self.read_chunks().await? {
            if chunk.last_seq() <= covered_to {
                let key =
                    Layout::tail_chunk(&self.cfg.tenant, self.generation, chunk.batch_id)?.to_key();
                match self.store.delete(&key).await {
                    Ok(()) | Err(StoreError::NotFound { .. }) => {}
                    Err(error) => return Err(error.into()),
                }
            } else if chunk.first_seq() <= covered_to {
                return Err(broken("a tail chunk straddles the manifest"));
            } else {
                remaining.push(chunk);
            }
        }
        let mut next_expected = covered_to + 1;
        for chunk in &remaining {
            if chunk.first_seq() != next_expected {
                return Err(broken("the tail chunks leave a gap after the manifest"));
            }
            next_expected = chunk.last_seq() + 1;
        }

        // 5. The relay's view.
        self.manifest = manifest;
        self.etag = etag;
        self.head_hash = head_hash;
        let recovered = !remaining.is_empty();
        if recovered {
            self.open = remaining
                .iter()
                .flat_map(|c| c.entries.iter().cloned())
                .collect();
            self.open_chunks = remaining.iter().map(|c| c.batch_id).collect();
            self.open_bytes = self
                .open
                .iter()
                .map(|(_, line)| 12 + u64::try_from(line.len()).unwrap_or(u64::MAX))
                .sum();
            if let Some(last) = remaining.last() {
                self.last_batch_id = self.last_batch_id.max(Some(last.batch_id));
            }
        } else {
            self.open_chunks.clear();
            if self.cfg.durability == Durability::RemoteSync {
                self.open.clear();
            } else {
                // Entries held in memory beyond the manifest stay; entries it covers are done,
                // and a remainder that does not start where the manifest ends cannot be sealed.
                self.open.retain(|(position, _)| position.seq > covered_to);
                if self
                    .open
                    .first()
                    .is_some_and(|(position, _)| position.seq != covered_to + 1)
                {
                    self.open.clear();
                }
            }
            self.open_bytes = self
                .open
                .iter()
                .map(|(_, line)| 12 + u64::try_from(line.len()).unwrap_or(u64::MAX))
                .sum();
        }
        if self.open.is_empty() {
            self.opened_at = None;
        }
        self.needs_resume = false;

        // 6. Re-seal the chunks the manifest does not cover yet.
        if recovered {
            let now = self.last_now;
            self.seal_open(now).await?;
        }
        Ok(())
    }

    async fn read_chunks(&self) -> Result<Vec<Chunk>, RelayError> {
        let prefix = format!(
            "{}/{}/{}/",
            self.cfg.tenant,
            TAIL_PREFIX,
            Layout::padded(self.generation)
        );
        let mut chunks = Vec::new();
        for entry in self.list_all(&prefix).await? {
            let Ok(Key::Tail(key)) = Layout::parse(&entry.key) else {
                return Err(broken(
                    "an object in the tail listing is not a tail chunk key",
                ));
            };
            let object = self
                .store
                .get(&entry.key)
                .await?
                .ok_or_else(|| broken("a listed tail chunk vanished"))?;
            let entries = chunk::decode(&object.bytes)
                .filter(|entries| self.chunk_is_consecutive(entries))
                .ok_or_else(|| broken("a tail chunk is damaged"))?;
            chunks.push(Chunk {
                batch_id: key.batch_id,
                entries,
            });
        }
        chunks.sort_by_key(|chunk| chunk.batch_id);
        Ok(chunks)
    }

    fn chunk_is_consecutive(&self, entries: &[Entry]) -> bool {
        let mut previous: Option<Position> = None;
        for (position, _) in entries {
            if position.generation != self.generation
                || previous.is_some_and(|before| before.seq.checked_add(1) != Some(position.seq))
            {
                return false;
            }
            previous = Some(*position);
        }
        previous.is_some()
    }
}
