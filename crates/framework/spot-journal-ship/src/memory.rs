//! In-memory implementations of the ports, for the shipper's own tests and for host tests that
//! must not need a database. Nothing here is durable.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use spot_events_contract::envelope::Position;

use crate::error::ShipFault;
use crate::ports::{
    Alarm, AlarmKind, Alarms, Mark, OutboxEntry, OutboxRead, OutboxSource, PortFuture,
    WatermarkStore, WriterLease,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An outbox held in a vector, with the generation rules of the real feed.
#[derive(Debug, Default)]
pub struct MemoryOutbox {
    entries: Mutex<Vec<OutboxEntry>>,
    down: AtomicBool,
}

impl MemoryOutbox {
    /// An empty outbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry.
    pub fn push(&self, entry: OutboxEntry) {
        lock(&self.entries).push(entry);
    }

    /// Appends entries `first..=last` of `generation`, each a small JSON line.
    pub fn push_range(&self, generation: u64, first: u64, last: u64) {
        for seq in first..=last {
            self.push(OutboxEntry {
                position: Position { generation, seq },
                line: format!("{{\"g\":{generation},\"seq\":{seq},\"data\":\"entry {seq}\"}}")
                    .into_bytes(),
            });
        }
    }

    /// Starts a new generation: the entries of older ones are gone (a restore).
    pub fn start_generation(&self, generation: u64) {
        lock(&self.entries).retain(|entry| entry.position.generation >= generation);
    }

    /// Removes the entries at or below `seq` of the current generation (pruning).
    pub fn prune_through(&self, seq: u64) {
        lock(&self.entries).retain(|entry| entry.position.seq > seq);
    }

    /// Makes reads fail, or work again.
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
}

impl OutboxSource for MemoryOutbox {
    fn read(&self, after: Position, limit: usize) -> PortFuture<'_, Result<OutboxRead, ShipFault>> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(ShipFault::OutboxUnavailable);
            }
            let entries = lock(&self.entries);
            let current = entries
                .iter()
                .map(|e| e.position.generation)
                .max()
                .unwrap_or(1);
            if after.generation != 0 && after.generation != current {
                return Ok(OutboxRead::Resync);
            }
            let floor = if after.generation == 0 { 0 } else { after.seq };
            let page: Vec<OutboxEntry> = entries
                .iter()
                .filter(|e| e.position.generation == current && e.position.seq > floor)
                .take(limit)
                .cloned()
                .collect();
            Ok(OutboxRead::Entries(page))
        })
    }
}

/// Watermarks held in a map.
#[derive(Debug, Default)]
pub struct MemoryWatermarks {
    marks: Mutex<HashMap<String, Mark>>,
    saves: AtomicU64,
    down: AtomicBool,
}

impl MemoryWatermarks {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The stored mark of `source`.
    #[must_use]
    pub fn get(&self, source: &str) -> Option<Mark> {
        lock(&self.marks).get(source).cloned()
    }

    /// How many saves succeeded.
    #[must_use]
    pub fn saves(&self) -> u64 {
        self.saves.load(Ordering::SeqCst)
    }

    /// Makes reads and writes fail, or work again.
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
}

impl WatermarkStore for MemoryWatermarks {
    fn load<'a>(&'a self, source: &'a str) -> PortFuture<'a, Result<Option<Mark>, ShipFault>> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(ShipFault::Watermark);
            }
            Ok(self.get(source))
        })
    }

    fn save<'a>(
        &'a self,
        source: &'a str,
        mark: &'a Mark,
        _now_millis: i64,
    ) -> PortFuture<'a, Result<(), ShipFault>> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(ShipFault::Watermark);
            }
            lock(&self.marks).insert(source.to_owned(), mark.clone());
            self.saves.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

/// A lease that is held until a test takes it away.
#[derive(Debug)]
pub struct MemoryLease {
    held: AtomicBool,
}

impl MemoryLease {
    /// A lease this node holds.
    #[must_use]
    pub fn held() -> Self {
        Self {
            held: AtomicBool::new(true),
        }
    }

    /// Gives the lease to another node (or back).
    pub fn set_held(&self, held: bool) {
        self.held.store(held, Ordering::SeqCst);
    }
}

impl WriterLease for MemoryLease {
    fn hold(&self) -> PortFuture<'_, bool> {
        Box::pin(async move { self.held.load(Ordering::SeqCst) })
    }

    fn release(&self) -> PortFuture<'_, ()> {
        Box::pin(async move { self.held.store(false, Ordering::SeqCst) })
    }
}

/// Alarms recorded in order: `(subject, kind, raised)`.
#[derive(Debug, Default)]
pub struct RecordingAlarms {
    events: Mutex<Vec<(String, AlarmKind, bool)>>,
    raised: Mutex<Vec<Alarm>>,
}

impl RecordingAlarms {
    /// No alarms yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every raise (`true`) and clear (`false`), oldest first.
    #[must_use]
    pub fn events(&self) -> Vec<(String, AlarmKind, bool)> {
        lock(&self.events).clone()
    }

    /// Whether `kind` is open on any subject (raised more recently than cleared).
    #[must_use]
    pub fn is_open(&self, kind: AlarmKind) -> bool {
        lock(&self.events)
            .iter()
            .rev()
            .find(|(_, k, _)| *k == kind)
            .is_some_and(|(_, _, raised)| *raised)
    }

    /// The alarms raised, with their data.
    #[must_use]
    pub fn alarms(&self) -> Vec<Alarm> {
        lock(&self.raised).clone()
    }
}

impl Alarms for RecordingAlarms {
    fn raise<'a>(&'a self, subject: &'a str, alarm: Alarm) -> PortFuture<'a, ()> {
        Box::pin(async move {
            lock(&self.events).push((subject.to_owned(), alarm.kind(), true));
            lock(&self.raised).push(alarm);
        })
    }

    fn clear<'a>(&'a self, subject: &'a str, kind: AlarmKind) -> PortFuture<'a, ()> {
        Box::pin(async move {
            lock(&self.events).push((subject.to_owned(), kind, false));
        })
    }
}
