//! In-memory sides and a fixed clock, for the tests and for callers that need a reference.
//!
//! Both sides digest a record the same way ([`body_digest`]): the canonical JSON of the document
//! body. A real adapter would digest the same canonical content on each side.

use std::cell::Cell;
use std::collections::BTreeMap;

use serde_json::Value;
use spot_events_contract::envelope::Position as FeedPosition;
use spot_journal_audit::{Fingerprint, Position, RecordSummary, content_digest};
use spot_journal_rebuild::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, SinkError, SinkFuture,
};

use crate::sides::{Clock, LiveSide, ScratchSide, Summaries};

/// The digest of a document body: the audit digest of its JSON text (object keys are sorted, so
/// the text is canonical).
#[must_use]
pub fn body_digest(body: &Value) -> [u8; 32] {
    content_digest(body.to_string().as_bytes())
}

/// One change the in-memory live side has seen.
#[derive(Clone, Debug)]
struct LiveChange {
    seq: u64,
    entity: String,
    id: String,
    version: u64,
    /// `None` is a delete.
    digest: Option<[u8; 32]>,
}

/// A live side that replays its own recorded changes up to the position it is asked for, so a
/// test can move the live database past the position an audit is pinned at.
#[derive(Clone, Debug)]
pub struct MemoryLive {
    tenant: String,
    changes: Vec<LiveChange>,
    head: Position,
    fingerprints: Vec<Fingerprint>,
}

impl MemoryLive {
    /// An empty live side of `tenant` at generation 1, position 0.
    #[must_use]
    pub fn new(tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            changes: Vec::new(),
            head: Position::new(1, 0),
            fingerprints: Vec::new(),
        }
    }

    /// Records an insert or update at `seq`; the head moves to `seq` when it is ahead.
    pub fn put(&mut self, seq: u64, entity: &str, id: &str, version: u64, body: &Value) {
        self.push(LiveChange {
            seq,
            entity: entity.to_owned(),
            id: id.to_owned(),
            version,
            digest: Some(body_digest(body)),
        });
    }

    /// Records a delete at `seq`; the head moves to `seq` when it is ahead.
    pub fn remove(&mut self, seq: u64, entity: &str, id: &str, version: u64) {
        self.push(LiveChange {
            seq,
            entity: entity.to_owned(),
            id: id.to_owned(),
            version,
            digest: None,
        });
    }

    fn push(&mut self, change: LiveChange) {
        if change.seq > self.head.seq {
            self.head = Position::new(self.head.generation, change.seq);
        }
        self.changes.push(change);
    }

    /// Sets the head without a change (for example a position that carries no state).
    pub fn set_head(&mut self, head: Position) {
        self.head = head;
    }

    /// Sets the fingerprints the live side reports (as of any position).
    pub fn set_fingerprints(&mut self, fingerprints: Vec<Fingerprint>) {
        self.fingerprints = fingerprints;
    }
}

impl LiveSide for MemoryLive {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn head(&self) -> Position {
        self.head
    }

    fn summaries(&self, as_of: Position) -> Summaries<'_> {
        let mut state: BTreeMap<(&str, &str), (u64, [u8; 32])> = BTreeMap::new();
        let mut ordered: Vec<&LiveChange> = self
            .changes
            .iter()
            .filter(|change| change.seq <= as_of.seq)
            .collect();
        ordered.sort_by_key(|change| change.seq);
        for change in ordered {
            let key = (change.entity.as_str(), change.id.as_str());
            match change.digest {
                Some(digest) => {
                    state.insert(key, (change.version, digest));
                }
                None => {
                    state.remove(&key);
                }
            }
        }
        Box::new(state.into_iter().map(|((entity, id), (version, digest))| {
            RecordSummary::new(entity, id, version, digest)
        }))
    }

    fn fingerprints(&self, _as_of: Position) -> Vec<Fingerprint> {
        self.fingerprints.clone()
    }
}

/// A scratch side that keeps the committed state in memory and counts how it was used.
#[derive(Debug)]
pub struct MemoryScratch {
    tenant: String,
    working: BTreeMap<(String, String), (u64, [u8; 32])>,
    committed: BTreeMap<(String, String), (u64, [u8; 32])>,
    fingerprints: Vec<Fingerprint>,
    discards: usize,
    aborts: usize,
}

impl MemoryScratch {
    /// An empty scratch side of `tenant`.
    #[must_use]
    pub fn new(tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            working: BTreeMap::new(),
            committed: BTreeMap::new(),
            fingerprints: Vec::new(),
            discards: 0,
            aborts: 0,
        }
    }

    /// Sets the fingerprints the scratch side reports.
    pub fn set_fingerprints(&mut self, fingerprints: Vec<Fingerprint>) {
        self.fingerprints = fingerprints;
    }

    /// How many times [`ScratchSide::discard`] was called.
    #[must_use]
    pub fn discard_count(&self) -> usize {
        self.discards
    }

    /// How many times the rebuild aborted.
    #[must_use]
    pub fn abort_count(&self) -> usize {
        self.aborts
    }

    /// How many records the scratch holds right now (committed plus uncommitted).
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.working.len() + self.committed.len()
    }
}

impl ApplySink for MemoryScratch {
    fn begin<'a>(&'a mut self, _summary: &'a PlanSummary) -> SinkFuture<'a> {
        self.working.clear();
        Box::pin(std::future::ready(Ok(())))
    }

    fn apply_image(&mut self, image: AppliedImage) -> SinkFuture<'_> {
        let digest = body_digest(&Value::Object(image.image.body().clone()));
        self.working
            .insert((image.entity, image.id), (image.version, digest));
        Box::pin(std::future::ready(Ok(())))
    }

    fn apply_delete(&mut self, delete: AppliedDelete) -> SinkFuture<'_> {
        self.working.remove(&(delete.entity, delete.id));
        Box::pin(std::future::ready(Ok(())))
    }

    fn apply_audit(&mut self, _audit: AppliedAudit) -> SinkFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn commit(&mut self, _last_position: FeedPosition) -> SinkFuture<'_> {
        self.committed = std::mem::take(&mut self.working);
        Box::pin(std::future::ready(Ok::<(), SinkError>(())))
    }

    fn abort<'a>(&'a mut self, _reason: &'a str) -> SinkFuture<'a> {
        self.aborts += 1;
        self.working.clear();
        Box::pin(std::future::ready(Ok(())))
    }
}

impl ScratchSide for MemoryScratch {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn summaries(&self) -> Summaries<'_> {
        Box::new(
            self.committed
                .iter()
                .map(|((entity, id), (version, digest))| {
                    RecordSummary::new(entity.as_str(), id.as_str(), *version, *digest)
                }),
        )
    }

    fn fingerprints(&self) -> Vec<Fingerprint> {
        self.fingerprints.clone()
    }

    fn discard(&mut self) {
        self.working.clear();
        self.committed.clear();
        self.discards += 1;
    }
}

/// A clock that advances by a fixed step on every read, so a run's duration is deterministic.
#[derive(Debug)]
pub struct SteppingClock {
    now: Cell<i64>,
    step: i64,
}

impl SteppingClock {
    /// A clock that starts at `start` and moves `step` milliseconds forward on each read.
    #[must_use]
    pub fn new(start: i64, step: i64) -> Self {
        Self {
            now: Cell::new(start),
            step,
        }
    }
}

impl Clock for SteppingClock {
    fn now_millis(&self) -> i64 {
        let now = self.now.get();
        self.now.set(now.saturating_add(self.step));
        now
    }
}
