//! Test doubles: scope, references, redaction, images and a queue shared across relay restarts.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, missing_docs)]

use serde_json::{Value, json};
use spot_sync_closure::{ClosureError, ImageSource, RecordKey, Redactor, References, SourceImage};
use spot_sync_hub::{
    Destination, DestinationScope, EntryRole, HubError, HubStream, MemoryQueue, PushOutcome,
    QueuedBundle, RelayConfig, SendQueue, StreamEntry,
};
use spot_sync_obligations::{Lane, Pending};
use spot_sync_skew::{NodeReport, SoftwareVersion};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

pub const TENANT: &str = "t1";
pub const SOURCE: &str = "hub";

pub fn k(entity: &str, id: &str) -> RecordKey {
    RecordKey::new(entity, id)
}

#[derive(Default, Clone)]
pub struct Scope {
    pub entities: BTreeSet<String>,
    pub ids: BTreeSet<RecordKey>,
    pub owned: BTreeSet<RecordKey>,
}

impl Scope {
    pub fn of(entities: &[&str]) -> Self {
        Self {
            entities: entities.iter().map(|s| (*s).to_owned()).collect(),
            ..Self::default()
        }
    }
}

impl DestinationScope for Scope {
    fn covers(&self, entry: &StreamEntry) -> bool {
        self.entities.contains(&entry.key.entity)
    }
    fn covers_key(&self, key: &RecordKey) -> bool {
        self.entities.contains(&key.entity) || self.ids.contains(key)
    }
    fn owns(&self, key: &RecordKey) -> bool {
        self.owned.contains(key)
    }
}

/// Removes the top-level `secret` field.
pub struct StripSecret;

impl Redactor for StripSecret {
    fn redact_down(&self, _entity: &str, image: &mut Value) {
        if let Some(o) = image.as_object_mut() {
            o.remove("secret");
        }
    }
}

/// A record's `rows[].to` values are references to a `master` record.
pub struct Refs;

impl References for Refs {
    fn references(&self, _entity: &str, image: &Value) -> Result<Vec<RecordKey>, ClosureError> {
        Ok(image
            .get("rows")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.get("to").and_then(Value::as_str))
                    .map(|id| RecordKey::new("master", id))
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[derive(Default, Clone)]
pub struct Src {
    pub images: BTreeMap<RecordKey, SourceImage>,
}

impl Src {
    pub fn put(&mut self, entity: &str, id: &str, image: Value, position: u64) {
        let key = RecordKey::new(entity, id);
        self.images
            .insert(key.clone(), SourceImage::new(key, image, position));
    }
}

impl ImageSource for Src {
    fn image(&self, key: &RecordKey) -> Result<Option<SourceImage>, ClosureError> {
        Ok(self.images.get(key).cloned())
    }
}

pub fn plain() -> Value {
    json!({ "note": "x", "secret": "s3cret" })
}

pub fn linking(to: &[&str]) -> Value {
    json!({ "rows": to.iter().map(|t| json!({ "to": t })).collect::<Vec<_>>(), "secret": "s3cret" })
}

pub fn report(majors: &[(&str, &[u32])]) -> NodeReport {
    NodeReport {
        software_version: SoftwareVersion::new(1, 0, 0),
        metadata_version: 1,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: majors
            .iter()
            .map(|(t, ms)| ((*t).to_owned(), ms.iter().copied().collect()))
            .collect(),
    }
}

/// Decodes major 1 of the topics named after these entities.
pub fn v1(entities: &[&str]) -> NodeReport {
    let m: Vec<(&str, &[u32])> = entities.iter().map(|e| (*e, &[1u32][..])).collect();
    report(&m)
}

pub fn dest(id: &str, entities: &[&str]) -> Destination {
    Destination::new(
        id,
        Box::new(Scope::of(entities)),
        Box::new(StripSecret),
        v1(&["item", "price", "order", "master", "tax"]),
    )
}

pub fn cfg() -> RelayConfig {
    RelayConfig::new(TENANT, SOURCE)
}

pub fn entry(position: u64, txn: &str, entity: &str, id: &str, version: u64) -> StreamEntry {
    StreamEntry {
        position,
        tenant: TENANT.to_owned(),
        source: SOURCE.to_owned(),
        origin: None,
        txn: txn.to_owned(),
        role: EntryRole::Root,
        key: RecordKey::new(entity, id),
        version,
        topic: entity.to_owned(),
        major: 1,
        lane: Lane::Bulk,
    }
}

/// A queue two relays can share, to model a restart that keeps the durable queue.
#[derive(Clone, Default)]
pub struct SharedQueue(pub Rc<RefCell<MemoryQueue>>);

impl SendQueue for SharedQueue {
    fn push(&mut self, dest: &str, item: Pending<QueuedBundle>) -> Result<PushOutcome, HubError> {
        self.0.borrow_mut().push(dest, item)
    }
    fn peek(&self, dest: &str, n: usize) -> Result<Vec<Pending<QueuedBundle>>, HubError> {
        self.0.borrow().peek(dest, n)
    }
    fn ack(&mut self, dest: &str, up_to: u64) -> Result<usize, HubError> {
        self.0.borrow_mut().ack(dest, up_to)
    }
    fn remove_redelivered(&mut self, dest: &str, position: u64) -> Result<usize, HubError> {
        self.0.borrow_mut().remove_redelivered(dest, position)
    }
    fn queued_bytes(&self, dest: &str) -> Result<usize, HubError> {
        self.0.borrow().queued_bytes(dest)
    }
}

/// The (record key, position, role) triples of everything queued, in serving order.
pub fn served(queue: &SharedQueue, dest: &str) -> Vec<(RecordKey, u64)> {
    queue
        .0
        .borrow()
        .all(dest)
        .iter()
        .flat_map(|p| {
            p.item
                .bundle
                .records
                .iter()
                .map(|r| (r.key.clone(), r.position))
        })
        .collect()
}

/// A compliant stream: entries after `after`, ascending, at most about `max` of them but always
/// whole transactions. The reference the violating doubles below depart from.
pub fn read_whole(entries: &[StreamEntry], after: u64, max: usize) -> Vec<StreamEntry> {
    let mut out: Vec<StreamEntry> = Vec::new();
    for e in entries.iter().filter(|e| e.position > after) {
        if out.len() >= max && out.last().is_some_and(|last| last.txn != e.txn) {
            break;
        }
        out.push(e.clone());
    }
    out
}

/// A stream whose read stops at the batch limit even when that cuts a source transaction in two:
/// the split-transaction violation of the `HubStream` contract, which the relay must refuse
/// (`sync_hub.stream_split_txn`).
#[derive(Debug, Clone, Default)]
pub struct SplittingStream {
    entries: Vec<StreamEntry>,
}

impl SplittingStream {
    /// The stream over `entries`, which must ascend.
    pub fn of(entries: Vec<StreamEntry>) -> Self {
        Self { entries }
    }
}

impl HubStream for SplittingStream {
    fn read_after(&self, after: u64, max: usize) -> Result<Vec<StreamEntry>, HubError> {
        Ok(self
            .entries
            .iter()
            .filter(|e| e.position > after)
            .take(max)
            .cloned()
            .collect())
    }

    fn head(&self) -> Result<u64, HubError> {
        Ok(self.entries.last().map_or(0, |e| e.position))
    }
}

/// A stream that interleaves source transactions (A, B, A, B) instead of keeping each whole: the
/// other way to break the contiguity of a transaction, without breaking the position order.
#[derive(Debug, Clone, Default)]
pub struct InterleavingStream {
    entries: Vec<StreamEntry>,
}

impl InterleavingStream {
    /// `txns` interleaved entry by entry, in the order given: the first entry of each transaction
    /// first, then the second of each, and so on. Positions are numbered from 1 in that order, so
    /// they still ascend; the transactions are what stops being contiguous.
    pub fn of(txns: Vec<Vec<StreamEntry>>) -> Self {
        let longest = txns.iter().map(Vec::len).max().unwrap_or(0);
        let mut entries = Vec::new();
        for i in 0..longest {
            for txn in &txns {
                if let Some(e) = txn.get(i) {
                    let mut e = e.clone();
                    e.position = u64::try_from(entries.len() + 1).unwrap_or(u64::MAX);
                    entries.push(e);
                }
            }
        }
        Self { entries }
    }
}

impl HubStream for InterleavingStream {
    fn read_after(&self, after: u64, max: usize) -> Result<Vec<StreamEntry>, HubError> {
        Ok(read_whole(&self.entries, after, max))
    }

    fn head(&self) -> Result<u64, HubError> {
        Ok(self.entries.last().map_or(0, |e| e.position))
    }
}

/// A stream in which one entity's entry carries another lane than the entity came in before: the
/// lane is no longer a function of the entity, which the relay must refuse
/// (`sync_hub.lane_not_entity`). Everything else about it is compliant.
#[derive(Debug, Clone, Default)]
pub struct LaneFlippingStream {
    entries: Vec<StreamEntry>,
}

impl LaneFlippingStream {
    /// `entries` with the lane of the entry at `position` flipped to the other lane.
    pub fn flipping_at(entries: Vec<StreamEntry>, position: u64) -> Self {
        let mut entries = entries;
        for e in &mut entries {
            if e.position == position {
                e.lane = match e.lane {
                    Lane::Mandatory => Lane::Bulk,
                    Lane::Bulk => Lane::Mandatory,
                };
            }
        }
        Self { entries }
    }
}

impl HubStream for LaneFlippingStream {
    fn read_after(&self, after: u64, max: usize) -> Result<Vec<StreamEntry>, HubError> {
        Ok(read_whole(&self.entries, after, max))
    }

    fn head(&self) -> Result<u64, HubError> {
        Ok(self.entries.last().map_or(0, |e| e.position))
    }
}

/// A compliant in-memory stream, built from entries.
pub fn memory_stream(entries: Vec<StreamEntry>) -> spot_sync_hub::MemoryStream {
    let mut s = spot_sync_hub::MemoryStream::new();
    for e in entries {
        s.append(e);
    }
    s
}
