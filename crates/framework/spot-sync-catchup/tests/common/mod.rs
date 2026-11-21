#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]
//! Shared fixtures: events, a topic table and a reference catalog for the tests.

use spot_sync_catchup::{
    CatchUpError, Coalescing, Event, EventClass, Payload, RecordId, References, StaticTopics,
};
use std::collections::BTreeMap;

/// Every fixture record is a `doc`.
pub const ENTITY: &str = "doc";

/// The record identity of `id`.
pub fn rid(id: &str) -> RecordId {
    RecordId::new(ENTITY, id)
}

/// A state event: the latest image of `id` at `position`, revision `version`.
pub fn state(position: u64, id: &str, version: u64) -> Event {
    Event::new(
        position,
        "doc_state",
        Payload::new(format!("{{\"id\":\"{id}\",\"version\":{version}}}")),
    )
    .with_subject(rid(id))
    .with_subject_version(version)
}

/// A domain event on a topic that keeps every event.
pub fn domain(position: u64, id: &str, version: u64) -> Event {
    Event::new(
        position,
        "doc_posted",
        Payload::new(format!("{{\"id\":\"{id}\",\"version\":{version}}}")),
    )
    .with_subject(rid(id))
    .with_subject_version(version)
}

/// A domain event on the topic that declares `coalesce: latest_per_subject`.
pub fn progress(position: u64, id: &str, version: u64) -> Event {
    Event::new(
        position,
        "task_progress",
        Payload::new(format!("{{\"id\":\"{id}\",\"version\":{version}}}")),
    )
    .with_subject(rid(id))
    .with_subject_version(version)
}

/// An audit event addressed to `addressee` (`None`: addressed to nobody).
pub fn audit(position: u64, addressee: Option<&str>) -> Event {
    let event = Event::new(position, "sys_sync_missing", Payload::new("{}"));
    match addressee {
        Some(node) => event.with_addressee(node),
        None => event,
    }
}

/// The topic table: `doc_state` is state, `doc_posted` domain, `task_progress` domain with
/// `latest_per_subject`, `sys_sync_missing` audit.
pub fn topics() -> StaticTopics {
    StaticTopics::new()
        .with("doc_state", EventClass::State)
        .with("doc_posted", EventClass::Domain)
        .with_coalescing(
            "task_progress",
            EventClass::Domain,
            Coalescing::LatestPerSubject,
        )
        .with("sys_sync_missing", EventClass::Audit)
}

/// A reference catalog keyed by fixture id: `link("invoice", "party")` says the invoice references
/// the party.
#[derive(Debug, Clone, Default)]
pub struct MapReferences {
    edges: BTreeMap<String, Vec<String>>,
}

impl MapReferences {
    /// An empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one reference, `from` referencing `to`.
    #[must_use]
    pub fn link(mut self, from: &str, to: &str) -> Self {
        self.edges
            .entry(from.to_owned())
            .or_default()
            .push(to.to_owned());
        self
    }

    /// The ids `record` references.
    #[must_use]
    pub fn targets(&self, record: &RecordId) -> Vec<String> {
        self.edges.get(&record.id).cloned().unwrap_or_default()
    }
}

impl References for MapReferences {
    fn references(
        &self,
        record: &RecordId,
        _image: &Payload,
    ) -> Result<Vec<RecordId>, CatchUpError> {
        Ok(self.targets(record).iter().map(|id| rid(id)).collect())
    }
}

/// A scope oracle over fixture ids: `in_scope` ids, and audit topics' payloads that "link" to one.
#[derive(Debug, Clone, Default)]
pub struct MapScope {
    held: Vec<String>,
    linked: Vec<u64>,
}

impl MapScope {
    /// A destination that holds `ids`.
    pub fn holding(ids: &[&str]) -> Self {
        Self {
            held: ids.iter().map(|id| (*id).to_owned()).collect(),
            linked: Vec::new(),
        }
    }

    /// The audit event at `position` is covered by reference.
    #[must_use]
    pub fn covering(mut self, position: u64) -> Self {
        self.linked.push(position);
        self
    }
}

impl spot_sync_catchup::ScopeOracle for MapScope {
    fn in_scope(&self, record: &RecordId) -> bool {
        self.held.contains(&record.id)
    }

    fn covers_by_reference(&self, event: &Event) -> bool {
        self.linked.contains(&event.position)
    }
}

/// An audit event about `id`.
pub fn audit_of(position: u64, id: &str) -> Event {
    Event::new(position, "sys_sync_missing", Payload::new("{}")).with_subject(rid(id))
}
