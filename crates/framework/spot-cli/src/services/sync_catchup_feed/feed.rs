//! The hub's catch-up feed: [`HubFeed`] answers the catch-up wiring's [`CatchupFeed`] questions
//! (what is this node's gap, and what is the coalesced plan for it) from what the hub holds,
//! through two narrow seams ([`FeedSource`] for the stores, [`Links`] for the record links), so the
//! logic is a plain library that a test drives over memory and production drives over PostgreSQL
//! ([`super::pg`]).
//!
//! # What `view` reports
//!
//! For a node the registry knows as **active with a subscription** (an unknown node, a node of
//! another tenant, a suspended or revoked node and a node with no readable subscription answer
//! `None`):
//!
//! - the node's [`NodeState`]: its identity from the registry, the site as tenant, the stream's
//!   generation and the applied prefix **the hub recorded from the node's acknowledgements** (never
//!   from a request body); a node that never acknowledged anything has `None`;
//! - the stream's [`HubHead`] (generation and head position, from the hub's own stream);
//! - the [`GapSummary`]: the in-scope events after the applied prefix and up to the head, counted
//!   by class (every forwarded event is a `doc_state` state event), the records and bytes a
//!   coalesced plan would carry, and the oldest position the hot outbox still holds.
//!
//! # A gap that cannot be planned is a bootstrap
//!
//! The plan is built (once, when the gap is read) only from the events the hub still holds in its
//! hot outbox. When that is not the whole gap, or the planner refuses it (a link the metadata
//! cannot resolve), the summary says `node_is_new_or_resync`, which the director turns into a
//! snapshot bootstrap (never a plan built from a partial read, and never a stored catch-up that
//! can serve nothing), and `plan` answers `None`:
//!
//! - the node never acknowledged anything, or claims a position above the hub's head;
//! - the node's prefix is below the oldest position the outbox holds (history lives only in the
//!   journal);
//! - the gap holds more rows than `max_events` (the bound on the work of one call);
//! - an event's payload does not decode, or the planner refuses the gap.
//!
//! A store that cannot answer is `None` from `view` (and a coded warning in the log): the director
//! then refuses the call, and nothing is planned on a guess.
//!
//! # What a plan carries
//!
//! The planner's input is the gap's events: for each in-scope event the stored event JSON
//! (`{"op", "entity_schema", "image"}`) with the node's `excluded_fields.down` removed from its
//! image, in position order. Scope is the node's own subscription and the policy's scope keys
//! ([`NodeScope::covers`]); it is applied once, before planning ([`scope_for_node`] also drops any
//! audit event not addressed to the node, as the planner requires). Record links come from the
//! site's metadata ([`Links`]), so a referenced record is never applied after its referrer.
//!
//! # Bounds and the cache
//!
//! One computation reads at most `max_events` rows (plus the rest of one transaction), the node's
//! registry row, the head and the oldest position: no lock is held across any of those reads. The
//! result is cached per node while its applied prefix and the stream generation are unchanged and
//! it was used within `cache_secs`, at most `max_nodes` nodes (the least recently used is evicted):
//! the chunk calls of one catch-up ask `view` and `plan` once each, and the same plan must give the
//! same chunks. A node that stays silent for longer than `cache_secs` in the middle of a plan is
//! planned again from the head of that moment.
//!
//! No event payload is ever put in a log line, an error or a metric label.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use spot_core::Clock;
use spot_sync_bootstrap::{HubHead, NodeState};
use spot_sync_catchup::{
    CatchUpError, CatchUpPlan, ClassCounts, Event, EventClass, GapSummary, Payload, RecordId,
    References, Settings, StaticTopics, plan, scope_for_node,
};

use crate::services::sync_catchup_wiring::{CatchupFeed, NodeView};

/// The only topic the hub forwards: a captured document image (a full-state event).
pub const TOPIC_DOC_STATE: &str = "doc_state";

/// A store could not answer. No detail: the adapter logs its own cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedFault;

/// One published outbox event, decoded once.
#[derive(Clone, Debug, PartialEq)]
pub struct RawEvent {
    /// The published position within the stream's generation.
    pub position: u64,
    /// The source transaction.
    pub txn: String,
    /// The ordinal within its transaction.
    pub ordinal: i32,
    /// The topic.
    pub topic: String,
    /// The subject entity.
    pub entity: String,
    /// The subject record id.
    pub id: String,
    /// The record's version after the event, when it carries one.
    pub version: Option<u64>,
    /// The event's schema version.
    pub schema_version: i32,
    /// The stored event JSON (`{"op", "entity_schema", "image"}`), or `Value::Null` when it did
    /// not decode (the gap is then not planned).
    pub body: Value,
}

impl RawEvent {
    /// Whether the event deleted its record (its `op` is `delete`).
    #[must_use]
    pub fn deleted(&self) -> bool {
        self.body.get("op").and_then(Value::as_str) == Some("delete")
    }
}

/// What the registry says about one node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeFacts {
    /// The journal prefix the hub knows the node applied (0 when none).
    pub applied_prefix: u64,
    /// Whether the node ever acknowledged anything (a serve sequence above zero).
    pub ever_acknowledged: bool,
}

/// The events after a position, as far as the bound allowed.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// The `doc_state` events read, ascending by position (empty when truncated).
    pub events: Vec<RawEvent>,
    /// More rows than the bound followed: nothing was kept.
    pub truncated: bool,
}

/// What one node may see: its subscription's scope and its `excluded_fields.down`.
pub trait NodeScope {
    /// Whether the node's subscription covers `event` (entity, scope keys, direction).
    fn covers(&self, event: &RawEvent) -> bool;

    /// Removes the excluded fields of `entity` from `image`, parent and child rows.
    fn redact(&self, entity: &str, image: &mut Value);
}

/// The hub's stores, as the feed reads them. Blocking.
pub trait FeedSource: Send + Sync {
    /// The stream's generation and head position.
    ///
    /// # Errors
    ///
    /// [`FeedFault`] when the store cannot answer.
    fn head(&self) -> Result<HubHead, FeedFault>;

    /// The oldest position the hot outbox holds, `None` when it holds nothing.
    ///
    /// # Errors
    ///
    /// [`FeedFault`] when the store cannot answer.
    fn oldest_position(&self) -> Result<Option<u64>, FeedFault>;

    /// The node's registry facts. `None` for a node that is unknown, of another tenant, not
    /// active or without a readable subscription.
    ///
    /// # Errors
    ///
    /// [`FeedFault`] when the store cannot answer.
    fn node(&self, node: &str) -> Result<Option<NodeFacts>, FeedFault>;

    /// The `doc_state` events with a position above `after`, at most about `max_rows` rows read
    /// (`truncated` when more followed).
    ///
    /// # Errors
    ///
    /// [`FeedFault`] when the store cannot answer.
    fn events_after(&self, after: u64, max_rows: usize) -> Result<Window, FeedFault>;

    /// The scope of `node` over `events` (which may serve to read scope keys without another
    /// read). `None` when the node has no scope now (no policy loaded: nothing may leave).
    ///
    /// # Errors
    ///
    /// [`FeedFault`] when the store cannot answer.
    fn scope(
        &self,
        node: &str,
        events: &[RawEvent],
    ) -> Result<Option<Box<dyn NodeScope>>, FeedFault>;
}

/// The links a record image declares, from the site's metadata.
pub trait Links: Send + Sync {
    /// The `(entity, id)` of every record `image` of `entity` references, `None` when a link's
    /// target cannot be determined from the metadata.
    fn references(&self, entity: &str, image: &Value) -> Option<Vec<(String, String)>>;
}

/// The feed's bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedSettings {
    /// The most outbox rows one computation reads.
    pub max_events: usize,
    /// How long an unused computation stays, in seconds.
    pub cache_secs: i64,
    /// How many nodes are kept.
    pub max_nodes: usize,
    /// The operator's estimate of a full scoped snapshot, in bytes; `None` when unknown (the cost
    /// rule then never prefers a snapshot to a coalesced plan).
    pub snapshot_bytes: Option<u64>,
}

impl Default for FeedSettings {
    fn default() -> Self {
        Self {
            max_events: 20_000,
            cache_secs: 120,
            max_nodes: 32,
            snapshot_bytes: None,
        }
    }
}

/// One node's computed gap, shared by the `view` and `plan` calls of one catch-up.
#[derive(Clone, Debug)]
pub struct Computed {
    /// The node's state.
    pub state: NodeState,
    /// The stream head the gap was read up to.
    pub head: HubHead,
    /// The gap's summary (a resync when no plan could be made).
    pub summary: GapSummary,
    /// The plan, built when the gap was read; `None` when the gap cannot be planned (the summary
    /// then says resync).
    pub plan: Option<CatchUpPlan>,
}

/// A read's answer, or `None` (with a coded warning, never a value) when the store could not.
fn answered<T>(read: Result<T, FeedFault>) -> Option<T> {
    match read {
        Ok(value) => Some(value),
        Err(FeedFault) => {
            tracing::warn!(
                code = "sync_catchup_feed.store_unavailable",
                "sync_catchup_feed: a store could not answer; the node is not planned now"
            );
            None
        }
    }
}

struct Entry {
    applied: u64,
    generation: u64,
    touched: i64,
    computed: Arc<Computed>,
}

/// The catch-up feed over a [`FeedSource`].
pub struct HubFeed {
    tenant: String,
    source: Arc<dyn FeedSource>,
    links: Arc<dyn Links>,
    planner: Settings,
    settings: FeedSettings,
    clock: Arc<dyn Clock>,
    cache: Mutex<BTreeMap<String, Entry>>,
}

impl HubFeed {
    /// A feed for `tenant` over `source`, planning with `planner` (the same settings the
    /// director routes by).
    #[must_use]
    pub fn new(
        tenant: &str,
        source: Arc<dyn FeedSource>,
        links: Arc<dyn Links>,
        planner: Settings,
        settings: FeedSettings,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tenant: tenant.to_owned(),
            source,
            links,
            planner,
            settings,
            clock,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fresh(&self, entry: &Entry, applied: u64, generation: u64, now: i64) -> bool {
        entry.applied == applied
            && entry.generation == generation
            && now.saturating_sub(entry.touched) <= self.settings.cache_secs
    }

    fn cached(&self, node: &str, applied: u64, generation: u64, now: i64) -> Option<Arc<Computed>> {
        let mut cache = self.lock();
        let entry = cache.get_mut(node)?;
        if !self.fresh(entry, applied, generation, now) {
            return None;
        }
        entry.touched = now;
        Some(Arc::clone(&entry.computed))
    }

    /// Keeps `computed`, unless a concurrent call kept one first (then that one is used, so every
    /// call of one catch-up sees the same plan).
    fn keep(
        &self,
        node: &str,
        applied: u64,
        generation: u64,
        now: i64,
        computed: Computed,
    ) -> Arc<Computed> {
        let mut cache = self.lock();
        if let Some(entry) = cache.get_mut(node)
            && self.fresh(entry, applied, generation, now)
        {
            entry.touched = now;
            return Arc::clone(&entry.computed);
        }
        while cache.len() >= self.settings.max_nodes && !cache.contains_key(node) {
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            cache.remove(&oldest);
        }
        let computed = Arc::new(computed);
        cache.insert(
            node.to_owned(),
            Entry {
                applied,
                generation,
                touched: now,
                computed: Arc::clone(&computed),
            },
        );
        computed
    }

    /// The node's computed gap, from the cache or from the stores. `None` for a node the hub
    /// does not know and for a store that cannot answer.
    fn computed(&self, node: &str) -> Option<Arc<Computed>> {
        let facts = answered(self.source.node(node))??;
        let head = answered(self.source.head())?;
        let now = self.now();
        if let Some(hit) = self.cached(node, facts.applied_prefix, head.generation, now) {
            return Some(hit);
        }
        let computed = self.build(node, facts, head)?;
        Some(self.keep(node, facts.applied_prefix, head.generation, now, computed))
    }

    fn build(&self, node: &str, facts: NodeFacts, head: HubHead) -> Option<Computed> {
        let never = !facts.ever_acknowledged && facts.applied_prefix == 0;
        let ahead = facts.applied_prefix > head.head_position;
        let applied = facts.applied_prefix.min(head.head_position);
        let state = NodeState {
            node_id: node.to_owned(),
            tenant: self.tenant.clone(),
            generation: head.generation,
            applied_position: if never { None } else { Some(applied) },
        };
        // History at and below `floor` is gone from the outbox: `oldest - 1` is the last position
        // a node may have applied and still be served from it.
        let oldest = answered(self.source.oldest_position())?;
        let floor = oldest
            .map_or(0, |position| position.saturating_sub(1))
            .min(head.head_position);
        let mut planned: Option<(GapSummary, CatchUpPlan)> = None;
        if !never && !ahead && applied >= floor {
            let mut events: Vec<Event> = Vec::new();
            let mut readable = true;
            if applied < head.head_position {
                let window = answered(self.source.events_after(applied, self.settings.max_events))?;
                if window.truncated {
                    readable = false;
                } else {
                    let scope = answered(self.source.scope(node, &window.events))?;
                    let Some(scope) = scope else {
                        // No scope: nothing may be sent to this node on a guess.
                        tracing::warn!(
                            code = "sync_catchup_feed.no_scope",
                            "sync_catchup_feed: the node has no scope now; it is not planned"
                        );
                        return None;
                    };
                    match events_of(&window.events, &*scope) {
                        Some(found) => events = found,
                        None => readable = false,
                    }
                }
            }
            if readable {
                let summary = self.summary(applied, floor, head, &events);
                // The plan is made now, with the director's own settings, so a gap the planner
                // refuses is a resync at the first question and never a stored catch-up that
                // can serve nothing.
                if let Some(plan) = self.plan_of(node, &summary, &events) {
                    planned = Some((summary, plan));
                }
            }
        }
        let (summary, plan) = match planned {
            Some((summary, plan)) => (summary, Some(plan)),
            None => (
                GapSummary {
                    applied_prefix: applied,
                    stream_end: head.head_position,
                    hot_retention_floor: floor,
                    in_scope_event_count: 0,
                    class_counts: ClassCounts::default(),
                    estimated_coalesced_records: 0,
                    estimated_coalesced_bytes: 0,
                    estimated_snapshot_bytes: self.settings.snapshot_bytes.unwrap_or(u64::MAX),
                    node_is_new_or_resync: true,
                },
                None,
            ),
        };
        Some(Computed {
            state,
            head,
            summary,
            plan,
        })
    }

    /// The coalesced plan for `events`, scoped for `node`; `None` when the planner refuses.
    fn plan_of(&self, node: &str, summary: &GapSummary, events: &[Event]) -> Option<CatchUpPlan> {
        let topics = StaticTopics::new().with(TOPIC_DOC_STATE, EventClass::State);
        let gap = scope_for_node(events, node, &topics);
        let refs = PayloadRefs(Arc::clone(&self.links));
        match plan(&gap, summary, &self.planner, &topics, &refs) {
            Ok(planned) => Some(planned),
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    "sync_catchup_feed: the planner refused the node's gap"
                );
                None
            }
        }
    }

    fn summary(&self, applied: u64, floor: u64, head: HubHead, events: &[Event]) -> GapSummary {
        let count = u64::try_from(events.len()).unwrap_or(u64::MAX);
        // What a coalesced plan carries: the latest image of each record.
        let mut latest: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for event in events {
            if let Some(record) = &event.subject {
                latest.insert(
                    (record.entity.as_str(), record.id.as_str()),
                    event.payload.bytes(),
                );
            }
        }
        let bytes: usize = latest.values().sum();
        GapSummary {
            applied_prefix: applied,
            stream_end: head.head_position,
            hot_retention_floor: floor,
            in_scope_event_count: count,
            class_counts: ClassCounts {
                state: count,
                domain: 0,
                audit: 0,
            },
            estimated_coalesced_records: u64::try_from(latest.len()).unwrap_or(u64::MAX),
            estimated_coalesced_bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
            estimated_snapshot_bytes: self.settings.snapshot_bytes.unwrap_or(u64::MAX),
            node_is_new_or_resync: false,
        }
    }
}

/// The planner's events for the in-scope rows, redacted for the node. `None` when a row's
/// payload did not decode (the gap is then not plannable).
fn events_of(rows: &[RawEvent], scope: &dyn NodeScope) -> Option<Vec<Event>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if row.topic != TOPIC_DOC_STATE {
            continue;
        }
        // A row that did not decode is not skipped as "out of scope": the scope cannot be
        // judged, so the gap is not planned.
        if row.body.is_null() {
            return None;
        }
        if !scope.covers(row) {
            continue;
        }
        let mut body = row.body.clone();
        if !row.deleted()
            && let Some(image) = body.get_mut("image")
        {
            scope.redact(&row.entity, image);
        }
        let mut event = Event::new(
            row.position,
            TOPIC_DOC_STATE,
            Payload::new(body.to_string()),
        )
        .with_subject(RecordId::new(row.entity.as_str(), row.id.as_str()));
        if let Some(version) = row.version {
            event = event.with_subject_version(version);
        }
        out.push(event);
    }
    Some(out)
}

/// The planner's reference catalogue over the stored event JSON: the links of the image, none for
/// a deletion.
struct PayloadRefs(Arc<dyn Links>);

impl References for PayloadRefs {
    fn references(
        &self,
        record: &RecordId,
        image: &Payload,
    ) -> Result<Vec<RecordId>, CatchUpError> {
        let unreadable = || CatchUpError::ReferenceUnreadable {
            entity: record.entity.clone(),
            id: record.id.clone(),
        };
        let body: Value = serde_json::from_str(image.as_str()).map_err(|_| unreadable())?;
        if body.get("op").and_then(Value::as_str) == Some("delete") {
            return Ok(Vec::new());
        }
        let Some(inner) = body.get("image") else {
            return Ok(Vec::new());
        };
        let found = self
            .0
            .references(&record.entity, inner)
            .ok_or_else(unreadable)?;
        Ok(found
            .into_iter()
            .map(|(entity, id)| RecordId::new(entity, id))
            .collect())
    }
}

impl CatchupFeed for HubFeed {
    fn view(&self, node: &str) -> Option<NodeView> {
        let computed = self.computed(node)?;
        Some(NodeView {
            state: computed.state.clone(),
            head: computed.head,
            summary: computed.summary,
        })
    }

    fn plan(&self, node: &str) -> Option<CatchUpPlan> {
        self.computed(node)?.plan.clone()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_body_that_did_not_decode_is_not_a_deletion() {
        let row = RawEvent {
            position: 1,
            txn: "t".to_owned(),
            ordinal: 0,
            topic: TOPIC_DOC_STATE.to_owned(),
            entity: "note".to_owned(),
            id: "a".to_owned(),
            version: Some(1),
            schema_version: 1,
            body: Value::Null,
        };
        assert!(!row.deleted());
        let gone = RawEvent {
            body: serde_json::json!({"op": "delete"}),
            ..row
        };
        assert!(gone.deleted());
    }

    #[test]
    fn the_default_bounds_are_the_documented_ones() {
        let defaults = FeedSettings::default();
        assert_eq!(defaults.max_events, 20_000);
        assert_eq!(defaults.cache_secs, 120);
        assert_eq!(defaults.max_nodes, 32);
        assert_eq!(defaults.snapshot_bytes, None);
    }
}
