//! Scoped audit-event lists (ADR-066 section 2, ADR-061 section 2; WP-SYN-07.T2).
//!
//! State events coalesce; audit events do not. An audit event is a record of what happened, so it
//! is delivered as an event, in position order, and only when it concerns something the
//! destination holds. Anything else would let a catch-up leak the existence of out-of-scope
//! records through audit lines, so the rules here are about what is *absent*:
//!
//! * which audit events are included is the [`AuditDelivery`] option pair (WP-SYN-07.T6). The
//!   default drops unaddressed events (ADR-066 section 2: never a broadcast) and delivers events
//!   addressed to this node whatever their subject; an event addressed to another node is never
//!   delivered. [`AuditDelivery::LEGACY`] is the pre-T6 rule: in scope (subject or by reference)
//!   and not addressed to another node;
//! * included audit events are never coalesced and never reordered;
//! * an out-of-scope audit event leaves no trace in the plan, in the mode decision's reasons, or in
//!   the counts the destination can see. The hub's own total lives in [`AuditCounts`], which has a
//!   [`destination_facing`](AuditCounts::destination_facing) form that cannot carry it.

use crate::delivery::AuditDelivery;
use crate::error::CatchUpError;
use crate::event::{Event, EventClass, RecordId};
use crate::order::References;
use crate::plan::{CatchUpPlan, plan};
use crate::scope::scope_for_node;
use crate::settings::Settings;
use crate::summary::GapSummary;
use crate::topics::TopicDeclarations;
use serde::{Deserialize, Serialize};

/// What the destination holds, as the hub's scope policy sees it.
pub trait ScopeOracle {
    /// Whether the destination holds, or is entitled to hold, `record`.
    fn in_scope(&self, record: &RecordId) -> bool;

    /// Whether `event` names an in-scope record through a link rather than as its subject.
    fn covers_by_reference(&self, event: &Event) -> bool;
}

/// How many audit events the gap held: for the hub's cost estimate, and for the destination.
///
/// This type is **hub-side only**. It is never serialised to a destination; use
/// [`destination_facing`](Self::destination_facing).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditCounts {
    /// Audit events in the gap that are delivered under the chosen option: what the destination
    /// receives.
    pub visible: u64,
    /// Every audit event in the gap, in scope or not: the hub's own cost estimate.
    pub total: u64,
}

impl AuditCounts {
    /// The form a destination may see: the visible count and nothing else.
    #[must_use]
    pub fn destination_facing(&self) -> VisibleAuditCount {
        VisibleAuditCount {
            visible: self.visible,
        }
    }
}

/// The destination-facing audit count. It has no `total` field, so it cannot leak one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleAuditCount {
    /// Audit events the destination receives.
    pub visible: u64,
}

/// A catch-up plan with its scoped audit list, and the hub-side counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditedPlan {
    /// The plan to send. Its `audit` field holds the normal-lane audit events; mandatory audit
    /// events are in its `mandatory_lane`.
    pub plan: CatchUpPlan,
    /// The hub-side counts. Send only [`AuditCounts::destination_facing`] to the node.
    pub counts: AuditCounts,
}

/// Plan one node's catch-up with scoped audit lists, under the default [`AuditDelivery`].
///
/// **Behaviour change (WP-SYN-07.T6):** before T6 this delivered an audit event iff it was in scope
/// and not addressed to another node. It now uses [`AuditDelivery::default`]: unaddressed audit
/// events are dropped and events addressed to this node are delivered whether or not their
/// subject is in scope. Call [`plan_with_audit_options`] with [`AuditDelivery::LEGACY`] for the
/// old rule.
///
/// The non-audit part is exactly [`plan`]'s, run over the gap without its audit events (so an audit
/// event addressed to another node cannot reach the bulk either). A kept audit event in
/// `(applied_prefix, stream_end]` whose `mandatory` flag is set joins the mandatory lane, merged
/// by position with the lane's other events; every other kept event goes to `plan.audit`. Both are
/// in strict position order.
///
/// The mode is chosen on a summary whose audit count is the delivered one, so a dropped audit
/// event can neither tip the mode nor appear in its reasons. A snapshot-mode plan carries the same
/// audit list: audit events are not snapshot content.
///
/// # Errors
/// What [`plan`] refuses, plus `UnknownTopic` for an event whose topic has no class.
pub fn plan_with_audit(
    events: &[Event],
    summary: &GapSummary,
    settings: &Settings,
    topics: &impl TopicDeclarations,
    refs: &impl References,
    oracle: &impl ScopeOracle,
    node: &str,
) -> Result<AuditedPlan, CatchUpError> {
    plan_with_audit_options(
        events,
        summary,
        settings,
        topics,
        refs,
        oracle,
        node,
        AuditDelivery::default(),
    )
}

/// [`plan_with_audit`] with an explicit [`AuditDelivery`]: which audit events are delivered.
///
/// `counts.total` counts every audit event in the gap; `counts.visible` counts what is delivered
/// under `delivery`. An audit event delivered because it is addressed to this node, though out of
/// scope, is carried only in the audit list (or the mandatory lane): its subject never enters
/// coalescing, the bundle plan or the mode reasons.
///
/// # Errors
/// What [`plan_with_audit`] refuses.
#[allow(clippy::too_many_arguments)]
pub fn plan_with_audit_options(
    events: &[Event],
    summary: &GapSummary,
    settings: &Settings,
    topics: &impl TopicDeclarations,
    refs: &impl References,
    oracle: &impl ScopeOracle,
    node: &str,
    delivery: AuditDelivery,
) -> Result<AuditedPlan, CatchUpError> {
    summary.validate()?;

    let mut rest = Vec::with_capacity(events.len());
    let mut kept_audit: Vec<Event> = Vec::new();
    let mut counts = AuditCounts::default();
    for event in events {
        let class = topics
            .class(&event.topic)
            .ok_or_else(|| CatchUpError::UnknownTopic {
                topic: event.topic.clone(),
            })?;
        if class != EventClass::Audit {
            rest.push(event.clone());
            continue;
        }
        if event.position <= summary.applied_prefix || event.position > summary.stream_end {
            continue;
        }
        counts.total += 1;
        // The keep rule is `AuditDelivery::keeps`: never another node's, never a broadcast
        // unless asked for, and an event addressed to this node by the option's choice.
        if delivery.keeps(event, node, oracle) {
            counts.visible += 1;
            kept_audit.push(event.clone());
        }
    }
    kept_audit.sort_by_key(|event| event.position);

    // The cost estimate counts the audit events the node will receive, not the ones it will not.
    let non_audit = summary
        .in_scope_event_count
        .saturating_sub(summary.class_counts.audit);
    let mut effective = *summary;
    effective.class_counts.audit = counts.visible;
    effective.in_scope_event_count = non_audit.saturating_add(counts.visible);

    let gap = scope_for_node(&rest, node, topics);
    let mut catch_up = plan(&gap, &effective, settings, topics, refs)?;

    let (mandatory, normal): (Vec<Event>, Vec<Event>) =
        kept_audit.into_iter().partition(|event| event.mandatory);
    catch_up.mandatory_lane.extend(mandatory);
    catch_up.mandatory_lane.sort_by_key(|event| event.position);
    catch_up.audit = normal;

    Ok(AuditedPlan {
        plan: catch_up,
        counts,
    })
}
