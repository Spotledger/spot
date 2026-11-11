//! The relay: construction, batch intake and sealing.

use spot_events_contract::envelope::Position;
use spot_journal_manifest::{Fork, Manifest, SegmentSummary, VerifyingKeys};
use spot_journal_segment::{
    MAX_LINE_BYTES, SealState, SegmentConfig, SegmentWriter, genesis_hash, should_seal,
};
use spot_journal_store::{JournalStore, Layout, StoreError};

use crate::chunk::{self, Entry};
use crate::config::{Appended, Batch, Durability, RelayConfig};
use crate::error::RelayError;
use crate::signer::ManifestSigner;

/// The relay of one tenant and one source, writing into one generation.
pub struct Relay<S: JournalStore> {
    pub(crate) store: S,
    pub(crate) signer: Box<dyn ManifestSigner>,
    pub(crate) keys: VerifyingKeys,
    pub(crate) cfg: RelayConfig,
    pub(crate) generation: u64,
    /// The fork record that opened this generation, when it is not generation 1.
    pub(crate) fork: Option<Fork>,
    /// The manifest as the relay last wrote or read it.
    pub(crate) manifest: Manifest,
    /// The store's etag of that manifest.
    pub(crate) etag: String,
    /// Hash of the last sealed segment (the genesis value before the first).
    pub(crate) head_hash: [u8; 32],
    /// The entries of the open segment, in position order.
    pub(crate) open: Vec<Entry>,
    /// Uncompressed bytes of `open`, as the sealing policy counts them.
    pub(crate) open_bytes: u64,
    /// The tail chunks (batch ids) that hold `open` (remote-sync).
    pub(crate) open_chunks: Vec<u64>,
    pub(crate) opened_at: Option<i64>,
    pub(crate) last_batch_id: Option<u64>,
    /// The latest time the caller supplied; used where `resume` needs a time and has no argument.
    pub(crate) last_now: i64,
    /// A call failed part-way: the cached view of the store may be stale.
    pub(crate) needs_resume: bool,
    /// Another relay was detected: nothing more is written.
    pub(crate) halted: bool,
}

impl<S: JournalStore> Relay<S> {
    /// Opens generation 1 of the source: creates its signed manifest when the store has none,
    /// then resumes (see [`Relay::resume`]).
    ///
    /// `keys` verifies the manifest the store already holds; the signer's own key must be in it.
    ///
    /// # Errors
    /// `config_invalid`, `split_brain` when someone else creates the manifest first,
    /// `chain_broken` for a stored state that disagrees, `store_failed`.
    pub async fn open(
        store: S,
        signer: Box<dyn ManifestSigner>,
        keys: VerifyingKeys,
        cfg: RelayConfig,
        now_millis: i64,
    ) -> Result<Self, RelayError> {
        Self::open_generation(store, signer, keys, cfg, 1, None, now_millis).await
    }

    /// Opens the generation a fork record opens (ADR-061 s6: a point-in-time rebuild abandons a
    /// generation). The abandoned generation's manifest must be in the store; the new generation's
    /// manifest is created from it with `fork` recorded, or, when it exists already, must carry
    /// exactly this fork record.
    ///
    /// # Errors
    /// As [`Relay::open`], and `chain_broken` when the abandoned generation's manifest is missing,
    /// does not verify or cannot be forked at that position.
    pub async fn open_fork(
        store: S,
        signer: Box<dyn ManifestSigner>,
        keys: VerifyingKeys,
        cfg: RelayConfig,
        fork: Fork,
        now_millis: i64,
    ) -> Result<Self, RelayError> {
        let generation = fork.generation;
        Self::open_generation(store, signer, keys, cfg, generation, Some(fork), now_millis).await
    }

    async fn open_generation(
        store: S,
        signer: Box<dyn ManifestSigner>,
        keys: VerifyingKeys,
        cfg: RelayConfig,
        generation: u64,
        fork: Option<Fork>,
        now_millis: i64,
    ) -> Result<Self, RelayError> {
        cfg.validate()?;
        if generation == 0 {
            return Err(RelayError::ConfigInvalid("generations start at 1"));
        }
        let manifest = Manifest::first(&cfg.source, now_millis)
            .map_err(|_| RelayError::ConfigInvalid("the source id is not usable"))?;
        let head_hash = genesis_hash(&cfg.source);
        let mut relay = Self {
            store,
            signer,
            keys,
            cfg,
            generation,
            fork,
            manifest,
            etag: String::new(),
            head_hash,
            open: Vec::new(),
            open_bytes: 0,
            open_chunks: Vec::new(),
            opened_at: None,
            last_batch_id: None,
            last_now: now_millis,
            needs_resume: true,
            halted: false,
        };
        relay.create_manifest_if_absent(now_millis).await?;
        relay.resume().await?;
        Ok(relay)
    }

    /// The tenant the relay writes for.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.cfg.tenant
    }

    /// The source the relay writes for.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.cfg.source
    }

    /// The generation the relay writes into.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The position the next entry must have.
    #[must_use]
    pub fn next_position(&self) -> Position {
        Position {
            generation: self.generation,
            seq: self.next_seq(),
        }
    }

    /// The manifest as the relay last wrote or read it.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// How many entries wait in the open (unsealed) segment.
    #[must_use]
    pub fn open_entries(&self) -> usize {
        self.open.len()
    }

    /// The store the relay writes to.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    pub(crate) fn next_seq(&self) -> u64 {
        match self.open.last() {
            Some((position, _)) => position.seq.saturating_add(1),
            None => self.manifest.next_seq(),
        }
    }

    pub(crate) fn halt(&mut self, reason: &'static str) -> RelayError {
        self.halted = true;
        RelayError::SplitBrain { reason }
    }

    fn ready(&self) -> Result<(), RelayError> {
        if self.halted {
            return Err(RelayError::SplitBrain {
                reason: "the relay stopped after it found another writer",
            });
        }
        if self.needs_resume {
            return Err(RelayError::ResumeRequired);
        }
        Ok(())
    }

    pub(crate) fn manifest_key(&self) -> Result<String, RelayError> {
        Ok(Layout::manifest(&self.cfg.tenant, self.generation)?.to_key())
    }

    /// Accepts a batch: all of it or none of it.
    ///
    /// Order of work: the batch is checked (nothing is written for a bad batch); a due open
    /// segment is sealed; under `remote-sync` the tail chunk is written; only then does the call
    /// return [`Appended::ChunkDurable`].
    ///
    /// # Errors
    /// `entry_invalid`, `entry_out_of_order` (a repeat or a step back), `batch_gap` (positions
    /// skipped), `resume_required`, `split_brain`, `chain_broken`, `store_failed`.
    pub async fn append_batch(
        &mut self,
        batch: Batch,
        now_millis: i64,
    ) -> Result<Appended, RelayError> {
        self.ready()?;
        self.last_now = self.last_now.max(now_millis);
        let up_to_seq = self.check_batch(&batch)?;
        if self.is_due(now_millis) {
            self.seal_open(now_millis).await?;
        }
        let remote = self.cfg.durability == Durability::RemoteSync;
        if remote {
            let bytes = chunk::encode(&batch.entries)
                .ok_or(RelayError::EntryInvalid("the batch is too large"))?;
            let key =
                Layout::tail_chunk(&self.cfg.tenant, self.generation, batch.batch_id)?.to_key();
            self.needs_resume = true;
            match self.store.put_new(&key, bytes).await {
                Ok(_) => {}
                Err(StoreError::AlreadyExists { .. }) => {
                    return Err(self.halt("a tail chunk with this batch id already exists"));
                }
                Err(error) => return Err(error.into()),
            }
            self.needs_resume = false;
            self.open_chunks.push(batch.batch_id);
        }
        for (_, line) in &batch.entries {
            self.open_bytes = self
                .open_bytes
                .saturating_add(12)
                .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
        }
        self.opened_at.get_or_insert(now_millis);
        self.last_batch_id = Some(batch.batch_id);
        let batch_id = batch.batch_id;
        self.open.extend(batch.entries);
        Ok(if remote {
            Appended::ChunkDurable {
                batch_id,
                up_to_seq,
            }
        } else {
            Appended::Buffered {
                batch_id,
                up_to_seq,
            }
        })
    }

    /// Seals the open segment if the sealing limits say so. Call it on a timer so an idle tenant's
    /// last entries are sealed too. Returns whether a segment was sealed.
    ///
    /// # Errors
    /// As [`Relay::append_batch`] for the sealing steps.
    pub async fn seal_if_due(&mut self, now_millis: i64) -> Result<bool, RelayError> {
        self.ready()?;
        self.last_now = self.last_now.max(now_millis);
        if !self.is_due(now_millis) {
            return Ok(false);
        }
        self.seal_open(now_millis).await?;
        Ok(true)
    }

    /// Seals the open segment now whatever the limits say (for a clean shutdown). Returns whether
    /// there was anything to seal.
    ///
    /// # Errors
    /// As [`Relay::append_batch`] for the sealing steps.
    pub async fn seal_now(&mut self, now_millis: i64) -> Result<bool, RelayError> {
        self.ready()?;
        self.last_now = self.last_now.max(now_millis);
        if self.open.is_empty() {
            return Ok(false);
        }
        self.seal_open(now_millis).await?;
        Ok(true)
    }

    fn is_due(&self, now_millis: i64) -> bool {
        let state = SealState {
            entries: u64::try_from(self.open.len()).unwrap_or(u64::MAX),
            bytes: self.open_bytes,
            opened_at_millis: self.opened_at.unwrap_or(now_millis),
        };
        should_seal(&state, now_millis, &self.cfg.limits)
    }

    /// Checks a batch against the relay's state without writing; returns its last sequence number.
    fn check_batch(&self, batch: &Batch) -> Result<u64, RelayError> {
        if batch.entries.is_empty() {
            return Err(RelayError::EntryInvalid("the batch is empty"));
        }
        if self
            .last_batch_id
            .is_some_and(|last| batch.batch_id <= last)
        {
            return Err(RelayError::EntryInvalid(
                "the batch id is not after the last batch id",
            ));
        }
        let mut expected = self.next_seq();
        for (position, line) in &batch.entries {
            if position.generation != self.generation {
                return Err(RelayError::EntryInvalid(
                    "a position is not in the relay's generation",
                ));
            }
            if std::str::from_utf8(line).is_err() {
                return Err(RelayError::EntryInvalid("a line is not valid UTF-8"));
            }
            if line.contains(&b'\n') || line.contains(&b'\r') {
                return Err(RelayError::EntryInvalid("a line contains a line break"));
            }
            if line.len() > MAX_LINE_BYTES {
                return Err(RelayError::EntryInvalid("a line is too large"));
            }
            if position.seq < expected {
                return Err(RelayError::EntryOutOfOrder {
                    expected,
                    offered: position.seq,
                });
            }
            if position.seq > expected {
                return Err(RelayError::BatchGap {
                    expected,
                    offered: position.seq,
                });
            }
            expected = expected
                .checked_add(1)
                .ok_or(RelayError::EntryInvalid("the sequence number overflows"))?;
        }
        Ok(expected - 1)
    }

    /// Seals the open entries: segment, then manifest, then chunk clean-up. Any error leaves
    /// `needs_resume` set, because the store may hold any prefix of the three steps.
    pub(crate) async fn seal_open(&mut self, now_millis: i64) -> Result<(), RelayError> {
        let (Some((first, _)), Some((last, _))) = (self.open.first(), self.open.last()) else {
            return Ok(());
        };
        let (first_seq, last_seq) = (first.seq, last.seq);
        self.needs_resume = true;

        let broken = |reason| RelayError::ChainBroken { reason };
        let mut writer = SegmentWriter::new(SegmentConfig::after(
            &self.cfg.source,
            self.generation,
            self.head_hash,
        ))
        .map_err(|_| broken("the segment writer refused its configuration"))?;
        for (position, line) in &self.open {
            writer
                .push(*position, line)
                .map_err(|_| broken("the open entries cannot be sealed into a segment"))?;
        }
        let sealed = writer
            .seal(Vec::<String>::new())
            .map_err(|_| broken("the segment could not be sealed"))?;

        // (a) the segment object.
        let segment_key =
            Layout::segment(&self.cfg.tenant, self.generation, first_seq, last_seq)?.to_key();
        match self.store.put_new(&segment_key, sealed.bytes.clone()).await {
            Ok(_) => {}
            Err(StoreError::AlreadyExists { .. }) => {
                let same = self
                    .store
                    .head(&segment_key)
                    .await?
                    .is_some_and(|meta| meta.etag == hex(&sealed.hash));
                if !same {
                    return Err(self.halt("a different segment already holds these positions"));
                }
            }
            Err(error) => return Err(error.into()),
        }

        // (b) the manifest, guarded by the etag read now.
        let manifest_key = self.manifest_key()?;
        let current = self
            .store
            .get(&manifest_key)
            .await?
            .ok_or_else(|| broken("the manifest is missing"))?;
        if current.meta.etag != self.etag {
            return Err(self.halt("the manifest changed since the relay last wrote it"));
        }
        let summary = SegmentSummary::new(
            self.generation,
            first_seq,
            last_seq,
            sealed.header.entry_count,
            sealed.hash,
            self.head_hash,
        );
        let mut next = self
            .manifest
            .append_segments(&[summary])
            .map_err(|_| broken("the segment does not extend the manifest"))?;
        next.issued_at_millis = now_millis;
        let signed = self.signer.sign(&next);
        match self
            .store
            .put_replace(&manifest_key, signed.to_bytes(), &current.meta.etag)
            .await
        {
            Ok(meta) => {
                self.manifest = next;
                self.etag = meta.etag;
                self.head_hash = sealed.hash;
            }
            Err(StoreError::EtagMismatch { .. }) => {
                return Err(self.halt("the manifest replace lost to another writer"));
            }
            Err(StoreError::NotFound { .. }) => return Err(broken("the manifest is missing")),
            Err(error) => return Err(error.into()),
        }

        // (c) the tail chunks the segment covers.
        for batch_id in self.open_chunks.clone() {
            let key = Layout::tail_chunk(&self.cfg.tenant, self.generation, batch_id)?.to_key();
            match self.store.delete(&key).await {
                Ok(()) | Err(StoreError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.open.clear();
        self.open_bytes = 0;
        self.open_chunks.clear();
        self.opened_at = None;
        self.needs_resume = false;
        Ok(())
    }
}

/// Lower-case hex of a hash: the form a store reports as an etag.
pub(crate) fn hex(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in hash {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}
