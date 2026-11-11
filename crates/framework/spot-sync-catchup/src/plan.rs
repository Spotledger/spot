//! The plan the hub sends to a node (ADR-066 section 1, step 2; section 2).
//!
//! The **mandatory lane** is planned first: mandatory events in the gap are delivered individually
//! and in order, ahead of all bulk, so a tax change is applied in the first moments of a session
//! whatever the backlog is. They are never coalesced away.
//!
//! The bulk then depends on the mode. Incremental sends the events in position order; coalesced
//! sends the latest image per record in dependency order; a snapshot is planned elsewhere, so the
//! bulk is the one snapshot bundle and the runtime produces it (section 3).

use crate::coalesce::coalesce;
use crate::error::CatchUpError;
use crate::event::{Event, EventClass, RecordId};
use crate::mode::{Mode, ModeReason, choose_mode};
use crate::order::{References, order_by_dependency};
use crate::scope::ScopedGap;
use crate::settings::Settings;
use crate::summary::GapSummary;
use crate::topics::TopicDeclarations;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What one planned bundle is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BundleKind {
    /// Events in position order, from an incremental replay.
    Incremental,
    /// Coalesced record images in dependency order: targets first, one group per bundle.
    CoalescedImages,
    /// Coalesced domain and addressed audit events, delivered after the images in position order.
    CoalescedFacts,
    /// The snapshot bundle: the runtime builds it at the plan's position (ADR-066 section 3).
    Snapshot,
}

/// One bundle the hub will build and send (ADR-064 section 1: applied in one destination
/// transaction).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleSpec {
    /// What the bundle carries and why.
    pub kind: BundleKind,
    /// The records it carries, in the order they go inside it.
    pub records: Vec<RecordId>,
    /// The events it carries. Empty for a snapshot bundle, which the runtime fills from the store.
    pub events: Vec<Event>,
    /// Estimated payload bytes: the sum of the events' payloads, or the snapshot estimate.
    pub bytes: u64,
}

/// What to send a node that returns after a gap, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchUpPlan {
    /// The chosen mode.
    pub mode: Mode,
    /// Every rule that fired, in evaluation order; the last one decided the mode.
    pub reasons: Vec<ModeReason>,
    /// The mandatory lane: individually, in position order, before all bulk.
    pub mandatory_lane: Vec<Event>,
    /// The bulk, in the order it must be applied.
    pub bulk: Vec<BundleSpec>,
    /// The in-scope audit events of the normal lane, in strict position order, never coalesced.
    /// Empty from [`plan`]; [`crate::plan_with_audit`] fills it (WP-SYN-07.T2).
    pub audit: Vec<Event>,
}

/// Plan one node's catch-up.
///
/// `gap` is the gap as the caller read it (from the hot outbox where it can, from journal
/// segments beyond it), already scoped for the node being served by
/// [`scope_for_node`](crate::scope_for_node): this function keeps the events in
/// `(applied_prefix, stream_end]`, so an event already applied, or beyond the end the summary
/// reports, is never planned twice. Scope is applied once, before planning; as defence in depth
/// the gap is re-checked here, in every mode, and a gap holding an audit event not addressed to
/// its node is refused (ADR-066 section 2: audit events are never broadcast).
///
/// # Errors
/// [`CatchUpError::UnscopedAudit`] for a foreign audit event, [`CatchUpError::UnknownTopic`] for
/// an event whose class cannot be told, and whatever [`choose_mode`], [`coalesce`] or [`order_by_dependency`] refuse: an unusable setting, an
/// inconsistent summary, an unregistered topic, a state event with no record, a duplicate image, or
/// an unreadable reference.
pub fn plan(
    gap: &ScopedGap,
    summary: &GapSummary,
    settings: &Settings,
    topics: &impl TopicDeclarations,
    refs: &impl References,
) -> Result<CatchUpPlan, CatchUpError> {
    let decision = choose_mode(summary, settings)?;
    gap.verify(topics)?;
    let node = gap.node();

    let mut in_gap: Vec<Event> = gap
        .events()
        .iter()
        .filter(|event| {
            event.position > summary.applied_prefix && event.position <= summary.stream_end
        })
        .cloned()
        .collect();
    in_gap.sort_by_key(|event| event.position);

    let mut mandatory_lane = Vec::new();
    let mut bulk_events = Vec::new();
    for event in in_gap {
        if event.mandatory {
            mandatory_lane.push(event);
        } else {
            bulk_events.push(event);
        }
    }

    let bulk = match decision.mode {
        Mode::Snapshot => vec![BundleSpec {
            kind: BundleKind::Snapshot,
            records: Vec::new(),
            events: Vec::new(),
            bytes: summary.estimated_snapshot_bytes,
        }],
        Mode::Incremental => bundles_of(
            bulk_events.chunks(settings.bundle_records),
            BundleKind::Incremental,
        ),
        Mode::Coalesced => coalesced_bulk(&bulk_events, settings, topics, refs, node)?,
    };

    Ok(CatchUpPlan {
        mode: decision.mode,
        reasons: decision.reasons,
        mandatory_lane,
        bulk,
        audit: Vec::new(),
    })
}

/// Turn chunked event slices into incremental bundles, records in payload order.
fn bundles_of<'a>(chunks: impl Iterator<Item = &'a [Event]>, kind: BundleKind) -> Vec<BundleSpec> {
    chunks
        .map(|chunk| BundleSpec {
            kind,
            records: chunk
                .iter()
                .filter_map(|event| event.subject.clone())
                .collect(),
            events: chunk.to_vec(),
            bytes: chunk.iter().map(|event| event.payload.bytes() as u64).sum(),
        })
        .collect()
}

/// The coalesced bulk: images in dependency order first, then the facts the node also needs.
fn coalesced_bulk(
    events: &[Event],
    settings: &Settings,
    topics: &impl TopicDeclarations,
    refs: &impl References,
    node: &str,
) -> Result<Vec<BundleSpec>, CatchUpError> {
    let coalesced = coalesce(events, topics, node)?;

    let mut images = Vec::new();
    let mut by_record: BTreeMap<RecordId, Event> = BTreeMap::new();
    let mut facts = Vec::new();
    for event in coalesced.events {
        match topics.class(&event.topic) {
            Some(EventClass::State) => {
                if let Some(image) = event.image() {
                    by_record.insert(image.record.clone(), event);
                    images.push(image);
                }
            }
            Some(EventClass::Domain | EventClass::Audit) => facts.push(event),
            None => {
                return Err(CatchUpError::UnknownTopic { topic: event.topic });
            }
        }
    }

    let groups = order_by_dependency(&images, refs)?;
    let mut bulk = Vec::with_capacity(groups.len() + 1);
    for group in groups {
        let mut records = Vec::with_capacity(group.len());
        let mut bundle_events = Vec::with_capacity(group.len());
        for record in group {
            if let Some(event) = by_record.get(&record) {
                bundle_events.push(event.clone());
            }
            records.push(record);
        }
        let bytes = bundle_events
            .iter()
            .map(|event| event.payload.bytes() as u64)
            .sum();
        bulk.push(BundleSpec {
            kind: BundleKind::CoalescedImages,
            records,
            events: bundle_events,
            bytes,
        });
    }
    bulk.extend(bundles_of(
        facts.chunks(settings.bundle_records),
        BundleKind::CoalescedFacts,
    ));
    Ok(bulk)
}
