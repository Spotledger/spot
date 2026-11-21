//! The single-pass, bounded-memory comparison.

use std::collections::{BTreeMap, HashMap};

use crate::error::CaptureInvariantError;
use crate::exclusions::{ExclusionKind, Exclusions};
use crate::model::{FeedEntry, Op, Position, StateEvent};
use crate::report::{ALL_KINDS, AppliedExclusion, Finding, FindingKind, Report};

/// The default window: tracked document versions.
pub const DEFAULT_WINDOW: usize = 65_536;

type Key = (String, String, u64, Op);

enum Slot {
    Feed(FeedEntry),
    Event(StateEvent),
    Matched { feed: Position, event: Position },
}

impl Slot {
    fn is_pending(&self) -> bool {
        !matches!(self, Self::Matched { .. })
    }
}

/// Compares the change feed with the `doc_state` events. Push both streams in position order,
/// in any interleaving, then call [`Checker::finish`].
pub struct Checker {
    exclusions: Exclusions,
    skipped: Vec<(u64, u64)>,
    window: usize,
    slots: HashMap<Key, (Slot, u64)>,
    order: BTreeMap<u64, Key>,
    arrivals: u64,
    pending: usize,
    peak_tracked: usize,
    peak_pending: usize,
    last_feed: Option<Position>,
    last_event: Option<Position>,
    feed_entries: u64,
    state_events: u64,
    matched: u64,
    findings: Vec<Finding>,
}

impl Checker {
    /// A checker with the given exclusions and window.
    ///
    /// # Errors
    ///
    /// `capture_invariant.window_invalid` when `window` is zero.
    pub fn new(exclusions: Exclusions, window: usize) -> Result<Self, CaptureInvariantError> {
        if window == 0 {
            return Err(CaptureInvariantError::WindowInvalid);
        }
        Ok(Self::build(exclusions, window))
    }

    fn build(exclusions: Exclusions, window: usize) -> Self {
        let skipped = vec![(0, 0); exclusions.items().len()];
        Self {
            exclusions,
            skipped,
            window,
            slots: HashMap::new(),
            order: BTreeMap::new(),
            arrivals: 0,
            pending: 0,
            peak_tracked: 0,
            peak_pending: 0,
            last_feed: None,
            last_event: None,
            feed_entries: 0,
            state_events: 0,
            matched: 0,
            findings: Vec::new(),
        }
    }

    /// A checker with no exclusions and [`DEFAULT_WINDOW`].
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::build(Exclusions::new(), DEFAULT_WINDOW)
    }

    /// Feed the next change-feed entry.
    ///
    /// # Errors
    ///
    /// `capture_invariant.stream_out_of_order` when the position does not increase;
    /// `capture_invariant.feed_duplicate` for a second entry of one document version.
    pub fn push_feed(&mut self, entry: FeedEntry) -> Result<(), CaptureInvariantError> {
        if self.last_feed.is_some_and(|last| entry.position <= last) {
            return Err(CaptureInvariantError::StreamOutOfOrder("change feed"));
        }
        self.last_feed = Some(entry.position);
        self.feed_entries += 1;
        if let Some(i) = self.exclusions.position_of(&entry.entity) {
            self.skipped[i].0 += 1;
            return Ok(());
        }
        let key = (
            entry.entity.clone(),
            entry.id.clone(),
            entry.version,
            entry.op,
        );
        match self.take(&key) {
            None => self.put(key, Slot::Feed(entry)),
            Some(Slot::Event(event)) => {
                self.matched += 1;
                self.put(
                    key,
                    Slot::Matched {
                        feed: entry.position,
                        event: event.position,
                    },
                );
            }
            Some(old) => {
                self.put(key, old);
                return Err(CaptureInvariantError::FeedDuplicate);
            }
        }
        Ok(())
    }

    /// Feed the next `doc_state` event.
    ///
    /// # Errors
    ///
    /// `capture_invariant.stream_out_of_order` when the position does not increase.
    pub fn push_event(&mut self, event: StateEvent) -> Result<(), CaptureInvariantError> {
        if self.last_event.is_some_and(|last| event.position <= last) {
            return Err(CaptureInvariantError::StreamOutOfOrder("journal"));
        }
        self.last_event = Some(event.position);
        self.state_events += 1;
        if let Some(i) = self.exclusions.position_of(&event.entity) {
            self.skipped[i].1 += 1;
            return Ok(());
        }
        let key = (
            event.entity.clone(),
            event.id.clone(),
            event.version,
            event.op,
        );
        match self.take(&key) {
            None => self.put(key, Slot::Event(event)),
            Some(Slot::Feed(entry)) => {
                self.matched += 1;
                self.put(
                    key,
                    Slot::Matched {
                        feed: entry.position,
                        event: event.position,
                    },
                );
            }
            Some(Slot::Event(first)) => {
                let mut f = finding(
                    FindingKind::DuplicateStateEvent,
                    &event.entity,
                    &event.id,
                    event.version,
                    None,
                    Some(&event),
                );
                f.first_event_position = Some(first.position);
                self.findings.push(f);
                self.put(key, Slot::Event(first));
            }
            Some(Slot::Matched { feed, event: first }) => {
                let mut f = finding(
                    FindingKind::DuplicateStateEvent,
                    &event.entity,
                    &event.id,
                    event.version,
                    None,
                    Some(&event),
                );
                f.feed_position = Some(feed);
                f.first_event_position = Some(first);
                self.findings.push(f);
                self.put(key, Slot::Matched { feed, event: first });
            }
        }
        Ok(())
    }

    /// End both streams and produce the report. What is still unmatched is definitive.
    #[must_use]
    pub fn finish(mut self) -> Report {
        for item in self.exclusions.items() {
            if item.kind == ExclusionKind::Derived && item.projector.is_none() {
                self.findings.push(Finding {
                    kind: FindingKind::DerivedWithoutProjector,
                    entity: item.entity.clone(),
                    id: String::new(),
                    version: 0,
                    event_version: None,
                    feed_position: None,
                    event_position: None,
                    first_event_position: None,
                    feed_op: None,
                    event_op: None,
                });
            }
        }
        // Pair what is left by document: a feed entry and an event for the same document with
        // different versions are one VersionMismatch, not a miss plus an orphan.
        type Pair = (Vec<FeedEntry>, Vec<StateEvent>);
        let mut by_doc: BTreeMap<(String, String), Pair> = BTreeMap::new();
        for (_, (slot, _)) in self.slots.drain() {
            match slot {
                Slot::Feed(e) => by_doc
                    .entry((e.entity.clone(), e.id.clone()))
                    .or_default()
                    .0
                    .push(e),
                Slot::Event(e) => by_doc
                    .entry((e.entity.clone(), e.id.clone()))
                    .or_default()
                    .1
                    .push(e),
                Slot::Matched { .. } => {}
            }
        }
        for (_, (mut feeds, mut events)) in by_doc {
            feeds.sort_by_key(|e| (e.version, e.position));
            events.sort_by_key(|e| (e.version, e.position));
            // Same version, different operation: not one document counted twice at two versions,
            // so each side is reported as its own miss or orphan (the key includes the op).
            let (same_f, feeds): (Vec<_>, Vec<_>) = feeds
                .into_iter()
                .partition(|f| events.iter().any(|e| e.version == f.version));
            let (same_e, events): (Vec<_>, Vec<_>) = events
                .into_iter()
                .partition(|e| same_f.iter().any(|f| f.version == e.version));
            for f in same_f {
                self.findings.push(finding(
                    FindingKind::MissingStateEvent,
                    &f.entity,
                    &f.id,
                    f.version,
                    Some(&f),
                    None,
                ));
            }
            for e in same_e {
                self.findings.push(finding(
                    FindingKind::OrphanStateEvent,
                    &e.entity,
                    &e.id,
                    e.version,
                    None,
                    Some(&e),
                ));
            }
            let mut feeds = feeds.into_iter();
            let mut events = events.into_iter();
            loop {
                match (feeds.next(), events.next()) {
                    (Some(f), Some(e)) => {
                        let mut found = finding(
                            FindingKind::VersionMismatch,
                            &f.entity,
                            &f.id,
                            f.version,
                            Some(&f),
                            Some(&e),
                        );
                        found.event_version = Some(e.version);
                        self.findings.push(found);
                    }
                    (Some(f), None) => self.findings.push(finding(
                        FindingKind::MissingStateEvent,
                        &f.entity,
                        &f.id,
                        f.version,
                        Some(&f),
                        None,
                    )),
                    (None, Some(e)) => self.findings.push(finding(
                        FindingKind::OrphanStateEvent,
                        &e.entity,
                        &e.id,
                        e.version,
                        None,
                        Some(&e),
                    )),
                    (None, None) => break,
                }
            }
        }
        self.findings.sort();
        let mut counts: BTreeMap<&'static str, u64> =
            ALL_KINDS.iter().map(|k| (k.code(), 0)).collect();
        for f in &self.findings {
            *counts.entry(f.kind.code()).or_insert(0) += 1;
        }
        let exclusions = self
            .exclusions
            .items()
            .iter()
            .zip(&self.skipped)
            .map(|(x, (feed, events))| AppliedExclusion {
                entity: x.entity.clone(),
                kind: x.kind,
                owner: x.owner.clone(),
                reason: x.reason.clone(),
                projector: x.projector.clone(),
                feed_entries_skipped: *feed,
                events_skipped: *events,
            })
            .collect();
        Report {
            window: self.window,
            feed_entries: self.feed_entries,
            state_events: self.state_events,
            matched: self.matched,
            peak_tracked: self.peak_tracked,
            peak_pending: self.peak_pending,
            counts,
            findings: self.findings,
            exclusions,
        }
    }

    fn take(&mut self, key: &Key) -> Option<Slot> {
        let (slot, stamp) = self.slots.remove(key)?;
        self.order.remove(&stamp);
        if slot.is_pending() {
            self.pending -= 1;
        }
        Some(slot)
    }

    fn put(&mut self, key: Key, slot: Slot) {
        self.arrivals += 1;
        if slot.is_pending() {
            self.pending += 1;
        }
        self.order.insert(self.arrivals, key.clone());
        self.slots.insert(key, (slot, self.arrivals));
        while self.slots.len() > self.window {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            let Some((slot, _)) = self.slots.remove(&oldest) else {
                continue;
            };
            match slot {
                Slot::Feed(e) => {
                    self.pending -= 1;
                    self.findings.push(finding(
                        FindingKind::OutOfWindow,
                        &e.entity,
                        &e.id,
                        e.version,
                        Some(&e),
                        None,
                    ));
                }
                Slot::Event(e) => {
                    self.pending -= 1;
                    self.findings.push(finding(
                        FindingKind::OutOfWindow,
                        &e.entity,
                        &e.id,
                        e.version,
                        None,
                        Some(&e),
                    ));
                }
                Slot::Matched { .. } => {}
            }
        }
        self.peak_tracked = self.peak_tracked.max(self.slots.len());
        self.peak_pending = self.peak_pending.max(self.pending);
    }
}

fn finding(
    kind: FindingKind,
    entity: &str,
    id: &str,
    version: u64,
    feed: Option<&FeedEntry>,
    event: Option<&StateEvent>,
) -> Finding {
    Finding {
        kind,
        entity: entity.to_owned(),
        id: id.to_owned(),
        version,
        event_version: None,
        feed_position: feed.map(|f| f.position),
        event_position: event.map(|e| e.position),
        first_event_position: None,
        feed_op: feed.map(|f| f.op),
        event_op: event.map(|e| e.op),
    }
}
