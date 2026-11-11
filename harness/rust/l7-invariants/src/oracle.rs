//! The oracle: what must be true of the store, built only from what the driver observed
//! committing.
//!
//! The driver describes each transaction attempt it ran as [`Effects`]. Effects of a committed
//! attempt are folded in with [`Oracle::apply`]; effects of an attempt whose commit outcome was
//! unknown wait until the driver has read the outcome back (by the attempt's marker event), so
//! the final oracle is exact. Attempts that did not commit contribute only their event ids, which
//! must never appear in the outbox.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use spot_storage_contract::{ChangeOp, Name, RecordId};

/// One document write of a transaction, net per document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocWrite {
    /// The document entity.
    pub entity: Name,
    /// The document.
    pub id: RecordId,
    /// Insert, update or delete.
    pub op: ChangeOp,
    /// The committed version (for a delete, the version deleted).
    pub version: u64,
}

/// Everything one transaction attempt would change if it commits.
#[derive(Clone, Debug)]
pub struct Effects {
    /// A number unique to this attempt; groups its outbox events.
    pub group: u64,
    /// Time before the attempt opened its transaction, microseconds since the run started.
    pub start_us: u64,
    /// Time after the attempt's commit returned.
    pub end_us: u64,
    /// Document writes.
    pub docs: Vec<DocWrite>,
    /// Counter documents incremented by one.
    pub increments: Vec<RecordId>,
    /// Sequence numbers allocated: `(series, number)`.
    pub numbers: Vec<(String, u64)>,
    /// Outbox event ids in enqueue order; the first is the attempt's marker.
    pub events: Vec<RecordId>,
    /// A read found the snapshot inconsistent: `(order id, detail)`.
    pub read_anomalies: Vec<(String, String)>,
}

impl Effects {
    /// Empty effects for attempt `group`.
    pub fn new(group: u64) -> Effects {
        Effects {
            group,
            start_us: 0,
            end_us: 0,
            docs: Vec::new(),
            increments: Vec::new(),
            numbers: Vec::new(),
            events: Vec::new(),
            read_anomalies: Vec::new(),
        }
    }
}

/// What the oracle knows about one document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocState {
    /// Created and not deleted.
    pub live: bool,
    /// Committed updates since the insert.
    pub updates: u64,
}

/// What the oracle expects of one outbox event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventExpect {
    /// The attempt that enqueued it.
    pub group: u64,
    /// Its enqueue order within that attempt.
    pub ordinal: u32,
}

/// The time span of a committed attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// Before the attempt opened its transaction.
    pub start_us: u64,
    /// After its commit returned.
    pub end_us: u64,
}

/// The expected final state of a run.
#[derive(Clone, Debug, Default)]
pub struct Oracle {
    docs: BTreeMap<(Name, RecordId), DocState>,
    counters: BTreeMap<RecordId, u64>,
    series: BTreeMap<String, BTreeSet<u64>>,
    duplicate_numbers: Vec<(String, u64)>,
    events: HashMap<RecordId, EventExpect>,
    aborted_events: HashSet<RecordId>,
    changes: BTreeMap<(Name, RecordId, u8, u64), u64>,
    deleted: BTreeSet<(Name, RecordId)>,
    windows: BTreeMap<u64, Window>,
    read_anomalies: Vec<(String, String)>,
}

fn op_code(op: ChangeOp) -> u8 {
    match op {
        ChangeOp::Insert => 0,
        ChangeOp::Update => 1,
        ChangeOp::Delete => 2,
    }
}

/// The inverse of the code used in [`Oracle::expected_changes`].
pub fn op_from_code(code: u8) -> ChangeOp {
    match code {
        0 => ChangeOp::Insert,
        1 => ChangeOp::Update,
        _ => ChangeOp::Delete,
    }
}

/// The key of [`ChangeOp`] in [`Oracle::expected_changes`].
pub fn change_code(op: ChangeOp) -> u8 {
    op_code(op)
}

impl Oracle {
    /// An empty oracle.
    pub fn new() -> Oracle {
        Oracle::default()
    }

    /// Folds in the effects of a committed attempt.
    pub fn apply(&mut self, fx: &Effects) {
        self.windows.insert(
            fx.group,
            Window {
                start_us: fx.start_us,
                end_us: fx.end_us,
            },
        );
        for w in &fx.docs {
            *self
                .changes
                .entry((w.entity.clone(), w.id, op_code(w.op), w.version))
                .or_insert(0) += 1;
            let key = (w.entity.clone(), w.id);
            match w.op {
                ChangeOp::Insert => {
                    self.docs.insert(
                        key,
                        DocState {
                            live: true,
                            updates: 0,
                        },
                    );
                }
                ChangeOp::Update => {
                    if let Some(state) = self.docs.get_mut(&key)
                        && state.live
                    {
                        state.updates += 1;
                    }
                }
                ChangeOp::Delete => {
                    if let Some(state) = self.docs.get_mut(&key) {
                        state.live = false;
                    }
                    self.deleted.insert(key);
                }
            }
        }
        for id in &fx.increments {
            *self.counters.entry(*id).or_insert(0) += 1;
        }
        for (series, number) in &fx.numbers {
            if !self
                .series
                .entry(series.clone())
                .or_default()
                .insert(*number)
            {
                self.duplicate_numbers.push((series.clone(), *number));
            }
        }
        for (ordinal, id) in fx.events.iter().enumerate() {
            self.events.insert(
                *id,
                EventExpect {
                    group: fx.group,
                    ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
                },
            );
        }
        self.read_anomalies
            .extend(fx.read_anomalies.iter().cloned());
    }

    /// Records the event ids of an attempt that did not commit: they must never be published.
    pub fn abort(&mut self, events: &[RecordId]) {
        for id in events {
            self.aborted_events.insert(*id);
        }
    }

    /// Registers a counter document created with value 0.
    pub fn track_counter(&mut self, id: RecordId) {
        self.counters.entry(id).or_insert(0);
    }

    /// The state of a document.
    pub fn doc(&self, entity: &Name, id: RecordId) -> Option<&DocState> {
        self.docs.get(&(entity.clone(), id))
    }

    /// Live documents of an entity, by id.
    pub fn live_docs<'a>(
        &'a self,
        entity: &'a Name,
    ) -> impl Iterator<Item = (RecordId, &'a DocState)> + 'a {
        self.docs
            .iter()
            .filter(move |((e, _), s)| e == entity && s.live)
            .map(|((_, id), s)| (*id, s))
    }

    /// Number of live documents of an entity: creates minus deletes.
    pub fn live_count(&self, entity: &Name) -> u64 {
        self.live_docs(entity).count() as u64
    }

    /// Successful increments of each counter.
    pub fn counters(&self) -> &BTreeMap<RecordId, u64> {
        &self.counters
    }

    /// The committed numbers of each series.
    pub fn series(&self) -> &BTreeMap<String, BTreeSet<u64>> {
        &self.series
    }

    /// Numbers handed out to two committed transactions: `(series, number)`.
    pub fn duplicate_numbers(&self) -> &[(String, u64)] {
        &self.duplicate_numbers
    }

    /// Committed outbox events.
    pub fn events(&self) -> &HashMap<RecordId, EventExpect> {
        &self.events
    }

    /// Whether `id` belonged to an attempt that did not commit.
    pub fn is_aborted_event(&self, id: &RecordId) -> bool {
        self.aborted_events.contains(id)
    }

    /// Expected change-feed entries: `(entity, document, op code, version)` and their counts.
    pub fn expected_changes(&self) -> &BTreeMap<(Name, RecordId, u8, u64), u64> {
        &self.changes
    }

    /// Whether the oracle recorded a committed delete of this document.
    pub fn was_deleted(&self, entity: &Name, id: RecordId) -> bool {
        self.deleted.contains(&(entity.clone(), id))
    }

    /// The time span of each committed attempt, by group.
    pub fn windows(&self) -> &BTreeMap<u64, Window> {
        &self.windows
    }

    /// Inconsistent snapshots seen by read operations: `(order id, detail)`.
    pub fn read_anomalies(&self) -> &[(String, String)] {
        &self.read_anomalies
    }
}
