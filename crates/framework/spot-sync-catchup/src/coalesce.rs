//! Coalescing (ADR-066 section 2, ADR-061 section 3): what a node that is behind actually needs.
//!
//! A node holds the **latest known state** of its mirrors, not their history (the history stays
//! with the owner and in the journal). So only **state** events collapse, and they collapse to the
//! latest image per record. Domain events are facts for consumers and are kept in full unless the
//! topic declares `coalesce: latest_per_subject`. Audit events are never broadcast: a node gets
//! only the ones addressed to it, and they are always kept individually.
//!
//! Nothing here is dropped silently: [`Coalesced`] reports what collapsed and what was suppressed.

use crate::error::CatchUpError;
use crate::event::{Event, EventClass, RecordId};
use crate::topics::{Coalescing, TopicDeclarations};
use std::collections::BTreeMap;

/// What coalescing kept, and what it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coalesced {
    /// The events to send, in position order.
    pub events: Vec<Event>,
    /// How many state or `latest_per_subject` events an earlier event made redundant.
    pub superseded: usize,
    /// How many audit events were dropped because they are addressed to another node.
    pub suppressed_audit: usize,
}

/// The rank of an event within its record: revision first, then the stream's own order.
///
/// A missing version counts as zero: an event with no revision can only be ordered by position.
fn rank(event: &Event) -> (u64, u64) {
    (event.subject_version.unwrap_or(0), event.position)
}

/// Collapse a gap's events to what the node still needs.
///
/// `node` is the node being served: an audit event is kept only when it is addressed to it. This
/// is the second line of defence: [`crate::plan`] takes a [`crate::ScopedGap`] and refuses a
/// foreign audit event before it gets here.
///
/// A **mandatory** event is never coalesced and never suppressed, even if a later image supersedes
/// it: the mandatory lane is delivered individually and in order (ADR-066 section 2).
///
/// # Errors
/// `UnknownTopic` when a topic has no registered class, and `SubjectMissing` when a state event
/// names no record.
pub fn coalesce(
    events: &[Event],
    topics: &impl TopicDeclarations,
    node: &str,
) -> Result<Coalesced, CatchUpError> {
    let mut ordered: Vec<Event> = events.to_vec();
    ordered.sort_by_key(|event| event.position);

    let mut class = Vec::with_capacity(ordered.len());
    for event in &ordered {
        let resolved = topics
            .class(&event.topic)
            .ok_or_else(|| CatchUpError::UnknownTopic {
                topic: event.topic.clone(),
            })?;
        if resolved == EventClass::State && event.subject.is_none() {
            return Err(CatchUpError::SubjectMissing {
                topic: event.topic.clone(),
                position: event.position,
            });
        }
        class.push(resolved);
    }

    // The winning index per record (state events) and per (topic, subject) (a domain topic that
    // declares `latest_per_subject`). Mandatory events take no part: all of them are kept.
    let mut state_winners: BTreeMap<RecordId, (u64, u64, usize)> = BTreeMap::new();
    let mut subject_winners: BTreeMap<(String, RecordId), (u64, u64, usize)> = BTreeMap::new();
    for (index, event) in ordered.iter().enumerate() {
        if event.mandatory {
            continue;
        }
        match class[index] {
            EventClass::State => {
                if let Some(record) = &event.subject {
                    let (version, position) = rank(event);
                    let better = state_winners
                        .get(record)
                        .is_none_or(|best| (version, position) > (best.0, best.1));
                    if better {
                        state_winners.insert(record.clone(), (version, position, index));
                    }
                }
            }
            EventClass::Domain => {
                if topics.coalescing(&event.topic) == Coalescing::LatestPerSubject
                    && let Some(record) = &event.subject
                {
                    let key = (event.topic.clone(), record.clone());
                    let (version, position) = rank(event);
                    let better = subject_winners
                        .get(&key)
                        .is_none_or(|best| (version, position) > (best.0, best.1));
                    if better {
                        subject_winners.insert(key, (version, position, index));
                    }
                }
            }
            EventClass::Audit => {}
        }
    }

    let mut kept = Vec::with_capacity(ordered.len());
    let mut superseded = 0usize;
    let mut suppressed_audit = 0usize;
    for (index, event) in ordered.iter().enumerate() {
        if event.mandatory {
            kept.push(event.clone());
            continue;
        }
        match class[index] {
            EventClass::State => match &event.subject {
                Some(record) if state_winners.get(record).map(|best| best.2) == Some(index) => {
                    kept.push(event.clone());
                }
                Some(_) => superseded += 1,
                None => kept.push(event.clone()),
            },
            EventClass::Domain => {
                let latest = topics.coalescing(&event.topic) == Coalescing::LatestPerSubject;
                let superseded_here = match &event.subject {
                    Some(record) if latest => {
                        let key = (event.topic.clone(), record.clone());
                        subject_winners.get(&key).map(|best| best.2) != Some(index)
                    }
                    _ => false,
                };
                if superseded_here {
                    superseded += 1;
                } else {
                    kept.push(event.clone());
                }
            }
            EventClass::Audit => {
                if event.addressed_to.as_deref() == Some(node) {
                    kept.push(event.clone());
                } else {
                    suppressed_audit += 1;
                }
            }
        }
    }

    Ok(Coalesced {
        events: kept,
        superseded,
        suppressed_audit,
    })
}
