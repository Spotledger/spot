//! What the topic registry declares about each topic (ADR-061 section 2, ADR-066 section 2).
//!
//! Two facts the planner needs come from the registry rather than from the events: the topic's
//! **class**, and whether a domain topic declares `coalesce: latest_per_subject`. The trait is the
//! seam, so this crate holds no list of topics.

use crate::event::EventClass;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Whether a topic's events collapse to the latest one per subject.
///
/// Only a **domain** topic can declare [`Coalescing::LatestPerSubject`] (ADR-066 section 2). State
/// topics always collapse per record, because a state event is a full image; audit topics are
/// never broadcast, so they never collapse. The declarations of those two classes are ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Coalescing {
    /// Every event is kept (the default).
    #[default]
    KeepAll,
    /// Only the latest event per subject is kept (`coalesce: latest_per_subject`).
    LatestPerSubject,
}

/// The registry's view of the topics in a gap.
pub trait TopicDeclarations {
    /// The class the topic is registered with, or `None` when it is not registered.
    ///
    /// A telemetry topic is not registered for sync at all: telemetry is never written to the
    /// outbox (ADR-061 section 2), so it has no class this planner can use and must answer `None`.
    fn class(&self, topic: &str) -> Option<EventClass>;

    /// Whether the topic declares `coalesce: latest_per_subject`. Kept events by default.
    fn coalescing(&self, _topic: &str) -> Coalescing {
        Coalescing::KeepAll
    }
}

/// A fixed declaration table: what a test, or a host that read the registry at boot, hands in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaticTopics {
    entries: BTreeMap<String, (EventClass, Coalescing)>,
}

impl StaticTopics {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a topic that keeps every event.
    #[must_use]
    pub fn with(mut self, topic: impl Into<String>, class: EventClass) -> Self {
        self.entries
            .insert(topic.into(), (class, Coalescing::KeepAll));
        self
    }

    /// Declare a topic and its coalescing rule.
    #[must_use]
    pub fn with_coalescing(
        mut self,
        topic: impl Into<String>,
        class: EventClass,
        coalescing: Coalescing,
    ) -> Self {
        self.entries.insert(topic.into(), (class, coalescing));
        self
    }
}

impl TopicDeclarations for StaticTopics {
    fn class(&self, topic: &str) -> Option<EventClass> {
        self.entries.get(topic).map(|(class, _)| *class)
    }

    fn coalescing(&self, topic: &str) -> Coalescing {
        self.entries
            .get(topic)
            .map_or(Coalescing::KeepAll, |(_, rule)| *rule)
    }
}
