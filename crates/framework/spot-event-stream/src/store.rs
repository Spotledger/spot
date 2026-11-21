//! [`ConsumerStore`]: the registry of consumers and their per-partition checkpoints.
//!
//! The trait is object-safe and asynchronous (boxed futures, so no macro crate is needed) and
//! the engine holds it as `Arc<dyn ConsumerStore>`. [`InMemoryConsumerStore`] is the reference
//! implementation; every backend must pass [`crate::contract::run_all`] (feature `testing`).
//!
//! A checkpoint moves only through [`ConsumerStore::advance`], a compare-and-set: the caller
//! presents the checkpoint it believes is current (`expected`) and the new one (`to`). A stale
//! writer (whose `expected` is no longer stored) is refused, and a move backwards is refused, so
//! a checkpoint never goes backwards or sideways. [`ConsumerStore::restart`] is the one
//! operation allowed to move a checkpoint backwards, and only after a resync.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use spot_events_contract::envelope::Position;
use time::OffsetDateTime;

use crate::error::StreamError;
use crate::position::{is_start, position_to_feed};
use crate::spec::ConsumerSpec;

/// A boxed, `Send` future: the return type of every store method.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StreamError>> + Send + 'a>>;

/// One registered consumer: its definition, the per-partition checkpoints and parking state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumerRecord {
    /// The definition.
    pub spec: ConsumerSpec,
    /// The acknowledged checkpoint of each partition (index = partition number).
    pub checkpoints: Vec<Position>,
    /// Whether each partition is parked (index = partition number).
    pub parked: Vec<bool>,
    /// When each parked partition was parked.
    pub parked_since: Vec<Option<OffsetDateTime>>,
    /// When the consumer was registered.
    pub registered_at: OffsetDateTime,
}

impl ConsumerRecord {
    /// A fresh record for `spec`, with every partition at its initial position.
    #[must_use]
    pub fn new(spec: ConsumerSpec, registered_at: OffsetDateTime) -> Self {
        let checkpoints = spec.initial_positions();
        let n = checkpoints.len();
        Self {
            spec,
            checkpoints,
            parked: vec![false; n],
            parked_since: vec![None; n],
            registered_at,
        }
    }

    /// The index of a partition, or [`StreamError::SpecInvalid`] when out of range.
    ///
    /// # Errors
    ///
    /// [`StreamError::SpecInvalid`] when the partition is not in `0..partitions`.
    pub fn index(&self, partition: u32) -> Result<usize, StreamError> {
        let index = partition as usize;
        if index >= self.checkpoints.len() {
            return Err(StreamError::spec(format!(
                "partition {partition} is outside 0..{}",
                self.spec.partitions
            )));
        }
        Ok(index)
    }
}

/// The outcome of [`ConsumerStore::register_if_absent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// The consumer did not exist; the given record was stored.
    Created,
    /// The consumer already existed; this is the record as stored (the given record was
    /// discarded and nothing was changed).
    Existing(Box<ConsumerRecord>),
}

/// The registry and checkpoints of every consumer of one site.
pub trait ConsumerStore: Send + Sync {
    /// Inserts or replaces a consumer record, resetting its checkpoints and parking state to
    /// the record's.
    ///
    /// This is the explicit "re-register with a new definition" path. It must **not** be used
    /// to create a consumer: a node that read no record and then calls `register` overwrites
    /// the progress another node acknowledged in between. Create with
    /// [`ConsumerStore::register_if_absent`].
    fn register<'a>(&'a self, record: &'a ConsumerRecord) -> StoreFuture<'a, ()>;

    /// Stores `record` only when no consumer of that name exists, atomically; otherwise leaves
    /// the stored consumer (spec, checkpoints, parking) untouched and returns it.
    ///
    /// There is deliberately no default implementation: a store that cannot do this
    /// atomically must not compile.
    fn register_if_absent<'a>(
        &'a self,
        record: &'a ConsumerRecord,
    ) -> StoreFuture<'a, RegisterOutcome>;

    /// Loads a consumer, if it exists.
    fn get<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<ConsumerRecord>>;

    /// Lists every consumer, in name order.
    fn list(&self) -> StoreFuture<'_, Vec<ConsumerRecord>>;

    /// The acknowledged checkpoint of one partition.
    fn checkpoint<'a>(&'a self, name: &'a str, partition: u32) -> StoreFuture<'a, Position>;

    /// Compare-and-set the checkpoint of one partition.
    ///
    /// Fails with `eventstream.ack_refused` when `expected` is not the stored checkpoint (a
    /// stale writer) or when `to` is behind `expected`. `to == expected` is an idempotent
    /// success.
    fn advance<'a>(
        &'a self,
        name: &'a str,
        partition: u32,
        to: Position,
        expected: Position,
    ) -> StoreFuture<'a, ()>;

    /// Moves a partition's checkpoint to `from`, backwards included. The only operation that
    /// may move a checkpoint backwards, used after a resync.
    fn restart<'a>(&'a self, name: &'a str, partition: u32, from: Position) -> StoreFuture<'a, ()>;

    /// Parks a partition, recording when.
    fn park<'a>(&'a self, name: &'a str, partition: u32, at: OffsetDateTime)
    -> StoreFuture<'a, ()>;

    /// Unparks a partition.
    fn unpark<'a>(&'a self, name: &'a str, partition: u32) -> StoreFuture<'a, ()>;

    /// Deletes a consumer and its checkpoints.
    fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()>;
}

/// The in-memory store, for tests and the single-node reference host.
#[derive(Debug, Default)]
pub struct InMemoryConsumerStore {
    consumers: RwLock<HashMap<String, ConsumerRecord>>,
}

impl InMemoryConsumerStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, ConsumerRecord>> {
        self.consumers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, ConsumerRecord>> {
        self.consumers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs `f` on the record named `name`, or fails with `eventstream.consumer_not_found`.
    fn with_record<T>(
        &self,
        name: &str,
        f: impl FnOnce(&mut ConsumerRecord) -> Result<T, StreamError>,
    ) -> Result<T, StreamError> {
        let mut guard = self.write();
        let record = guard
            .get_mut(name)
            .ok_or_else(|| StreamError::ConsumerNotFound {
                name: name.to_owned(),
            })?;
        f(record)
    }
}

impl ConsumerStore for InMemoryConsumerStore {
    fn register<'a>(&'a self, record: &'a ConsumerRecord) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.write()
                .insert(record.spec.name.clone(), record.clone());
            Ok(())
        })
    }

    fn register_if_absent<'a>(
        &'a self,
        record: &'a ConsumerRecord,
    ) -> StoreFuture<'a, RegisterOutcome> {
        Box::pin(async move {
            match self.write().entry(record.spec.name.clone()) {
                Entry::Occupied(existing) => {
                    Ok(RegisterOutcome::Existing(Box::new(existing.get().clone())))
                }
                Entry::Vacant(slot) => {
                    slot.insert(record.clone());
                    Ok(RegisterOutcome::Created)
                }
            }
        })
    }

    fn get<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<ConsumerRecord>> {
        Box::pin(async move { Ok(self.read().get(name).cloned()) })
    }

    fn list(&self) -> StoreFuture<'_, Vec<ConsumerRecord>> {
        Box::pin(async move {
            let mut records: Vec<ConsumerRecord> = self.read().values().cloned().collect();
            records.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
            Ok(records)
        })
    }

    fn checkpoint<'a>(&'a self, name: &'a str, partition: u32) -> StoreFuture<'a, Position> {
        Box::pin(async move {
            let guard = self.read();
            let record = guard
                .get(name)
                .ok_or_else(|| StreamError::ConsumerNotFound {
                    name: name.to_owned(),
                })?;
            let index = record.index(partition)?;
            Ok(record.checkpoints[index])
        })
    }

    fn advance<'a>(
        &'a self,
        name: &'a str,
        partition: u32,
        to: Position,
        expected: Position,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.with_record(name, |record| {
                let index = record.index(partition)?;
                let stored = record.checkpoints[index];
                if position_to_feed(stored) != position_to_feed(expected) {
                    return Err(StreamError::ack("the checkpoint moved since it was read"));
                }
                if position_to_feed(to) < position_to_feed(expected) {
                    return Err(StreamError::ack("a checkpoint never moves backwards"));
                }
                record.checkpoints[index] = to;
                Ok(())
            })
        })
    }

    fn restart<'a>(&'a self, name: &'a str, partition: u32, from: Position) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.with_record(name, |record| {
                let index = record.index(partition)?;
                record.checkpoints[index] = from;
                record.parked[index] = false;
                record.parked_since[index] = None;
                Ok(())
            })
        })
    }

    fn park<'a>(
        &'a self,
        name: &'a str,
        partition: u32,
        at: OffsetDateTime,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.with_record(name, |record| {
                let index = record.index(partition)?;
                if !record.parked[index] {
                    record.parked[index] = true;
                    record.parked_since[index] = Some(at);
                }
                Ok(())
            })
        })
    }

    fn unpark<'a>(&'a self, name: &'a str, partition: u32) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.with_record(name, |record| {
                let index = record.index(partition)?;
                record.parked[index] = false;
                record.parked_since[index] = None;
                Ok(())
            })
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.write().remove(name);
            Ok(())
        })
    }
}

/// A store handle the engine shares.
pub type SharedStore = Arc<dyn ConsumerStore>;

/// Whether a checkpoint is still at the feed's start (nothing acknowledged yet).
#[must_use]
pub fn never_acked(checkpoint: Position) -> bool {
    is_start(checkpoint)
}
