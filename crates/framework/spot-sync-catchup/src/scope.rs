//! The scoped gap: the only input the planner accepts (ADR-066 section 2; WP-SYN-07.T4).
//!
//! Audit events are never broadcast: a node receives only those addressed to it. A foreign audit
//! event in a plan (for example a handover adoption message, ADR-063) discloses a hub fact to the
//! wrong node, so scope is applied once, up front, by [`scope_for_node`], and [`crate::plan`]
//! takes a [`ScopedGap`] and nothing else. [`crate::plan`] also re-checks the gap and refuses
//! fail-closed (defence in depth).

use crate::error::CatchUpError;
use crate::event::{Event, EventClass};
use crate::topics::TopicDeclarations;

/// A gap's events already scoped for one node. Only [`scope_for_node`] builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedGap {
    events: Vec<Event>,
    node: String,
    dropped_audit: usize,
}

impl ScopedGap {
    /// The events, scoped for [`node`](Self::node): every non-audit event and the audit events
    /// addressed to that node.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The node the gap was scoped for.
    #[must_use]
    pub fn node(&self) -> &str {
        &self.node
    }

    /// How many audit events scoping dropped (addressed to another node, or to nobody).
    #[must_use]
    pub fn dropped_audit(&self) -> usize {
        self.dropped_audit
    }

    /// Build a gap without scoping it, so a unit test can hand the planner a leaking one. It is
    /// not public and exists only in the crate's own test build.
    #[cfg(test)]
    pub(crate) fn unscoped_for_test(events: Vec<Event>, node: &str) -> Self {
        Self {
            events,
            node: node.to_owned(),
            dropped_audit: 0,
        }
    }

    /// Fail closed unless every audit event is addressed to this gap's node and every topic has a
    /// class (an event of unknown class cannot be shown not to be audit).
    pub(crate) fn verify(&self, topics: &impl TopicDeclarations) -> Result<(), CatchUpError> {
        for event in &self.events {
            match topics.class(&event.topic) {
                None => {
                    return Err(CatchUpError::UnknownTopic {
                        topic: event.topic.clone(),
                    });
                }
                Some(EventClass::Audit)
                    if event.addressed_to.as_deref() != Some(self.node.as_str()) =>
                {
                    return Err(CatchUpError::UnscopedAudit {
                        position: event.position,
                    });
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

/// Scope `events` for `node`: keep every non-audit event and only the audit events with
/// `addressed_to == Some(node)`, and count the audit events dropped.
///
/// An event whose topic has no class is kept, because it cannot be shown to be audit here; the
/// planner then refuses it with `UnknownTopic`, in every mode.
///
/// `topics` is needed because the audit class is a topic declaration, not a field of the event.
#[must_use]
pub fn scope_for_node(events: &[Event], node: &str, topics: &impl TopicDeclarations) -> ScopedGap {
    let mut kept = Vec::with_capacity(events.len());
    let mut dropped_audit = 0usize;
    for event in events {
        let is_audit = topics.class(&event.topic) == Some(EventClass::Audit);
        if is_audit && event.addressed_to.as_deref() != Some(node) {
            dropped_audit += 1;
        } else {
            kept.push(event.clone());
        }
    }
    ScopedGap {
        events: kept,
        node: node.to_owned(),
        dropped_audit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Payload;
    use crate::mode::Mode;
    use crate::order::References;
    use crate::settings::Settings;
    use crate::summary::{ClassCounts, GapSummary};
    use crate::topics::StaticTopics;

    struct NoRefs;
    impl References for NoRefs {
        fn references(
            &self,
            _record: &crate::event::RecordId,
            _image: &Payload,
        ) -> Result<Vec<crate::event::RecordId>, CatchUpError> {
            Ok(Vec::new())
        }
    }

    fn topics() -> StaticTopics {
        StaticTopics::new()
            .with("doc_state", EventClass::State)
            .with("sys_adopt", EventClass::Audit)
    }

    fn leaking() -> ScopedGap {
        let events = vec![
            Event::new(1, "doc_state", Payload::new("{}"))
                .with_subject(crate::event::RecordId::new("doc", "a")),
            Event::new(2, "sys_adopt", Payload::new("{}")).with_addressee("other"),
        ];
        ScopedGap::unscoped_for_test(events, "me")
    }

    fn summary(mode: Mode) -> GapSummary {
        let snapshot = mode == Mode::Snapshot;
        GapSummary {
            applied_prefix: 0,
            stream_end: 10,
            hot_retention_floor: 0,
            in_scope_event_count: 2,
            class_counts: ClassCounts::default(),
            estimated_coalesced_records: 1,
            estimated_coalesced_bytes: 1,
            estimated_snapshot_bytes: 1_000_000,
            node_is_new_or_resync: snapshot,
        }
    }

    #[test]
    fn a_leaking_gap_is_refused_in_every_mode() {
        let coalesce_settings = Settings {
            incremental_limit: 1,
            ..Settings::default()
        };
        let cases = [
            (summary(Mode::Incremental), Settings::default()),
            (summary(Mode::Snapshot), Settings::default()),
            (summary(Mode::Incremental), coalesce_settings),
        ];
        for (summary, settings) in cases {
            let result = crate::plan(&leaking(), &summary, &settings, &topics(), &NoRefs);
            assert_eq!(
                result.map(|_| ()),
                Err(CatchUpError::UnscopedAudit { position: 2 })
            );
        }
    }
}
