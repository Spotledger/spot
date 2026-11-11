//! Plain data: stream entries, queue items, settings and the tick report.

use crate::error::HubError;
use serde::{Deserialize, Serialize};
use spot_sync_closure::{Bundle, Limits, RecordKey};
use spot_sync_obligations::Lane;
use std::collections::BTreeMap;

/// Whether an entry is a document the user saved or submitted, or another event of its source
/// transaction (ADR-064 section 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryRole {
    /// A saved or submitted document.
    Root,
    /// Another in-scope event of the same source transaction.
    Satellite,
}

/// One entry of the hub's stream: a reference to a record event, never its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEntry {
    /// Position in the hub stream. Positions start at 1 and strictly ascend.
    pub position: u64,
    /// The tenant the event belongs to. An entry of another tenant is dropped for everyone.
    pub tenant: String,
    /// The source (hub) stream the entry was written to. An entry of another source is dropped.
    pub source: String,
    /// The node the change came from (the up direction); it is never sent back there.
    pub origin: Option<String>,
    /// The source transaction. Entries of one transaction are bundled together.
    pub txn: String,
    /// Root or satellite of its transaction.
    pub role: EntryRole,
    /// The record.
    pub key: RecordKey,
    /// The record's version after the event.
    pub version: u64,
    /// The event topic (ADR-056).
    pub topic: String,
    /// The event's schema major.
    pub major: u32,
    /// The delivery lane. It must be a function of the record's entity, so a record's versions
    /// never swap places between lanes.
    pub lane: Lane,
}

/// One bundle of a send queue, with the identity that makes a push idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedBundle {
    /// The bundle's index within its transaction's plan (prerequisites first).
    pub index: u32,
    /// True for a record re-sent after a version-skew hold was released: it is not trimmed by a
    /// prefix acknowledgement, because the node's prefix passed it while it was held.
    pub redelivery: bool,
    /// The bundle, already redacted for the destination.
    pub bundle: Bundle,
}

/// What a push to the queue did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// The item was added.
    Queued,
    /// An item with the same `(destination, position, index, redelivery)` was already there.
    Duplicate,
}

/// Relay settings. Values are settings, never hidden constants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayConfig {
    /// The tenant this hub serves. Entries of any other tenant are dropped.
    pub tenant: String,
    /// The source stream this relay reads. Entries tagged with another source are dropped.
    pub source: String,
    /// A destination whose queue holds this many bytes is parked as backpressured.
    pub max_queue_bytes: usize,
    /// Most bundles one destination is given in one tick (default 50).
    pub max_bundles_per_tick_per_dest: usize,
    /// Most stream entries read per tick (whole transactions may exceed it).
    pub read_batch: usize,
    /// Most distinct entities the relay's lane map may hold: the map proves that an entry's lane
    /// is a function of its entity, so a stream that names more entities than this is refused
    /// (`sync_hub.stream_entity_limit`) rather than growing the map without bound.
    pub max_entities: usize,
    /// Closure bounds.
    pub limits: Limits,
}

impl RelayConfig {
    /// A configuration with the default bounds.
    #[must_use]
    pub fn new(tenant: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            source: source.into(),
            max_queue_bytes: 64 * 1024 * 1024,
            max_bundles_per_tick_per_dest: 50,
            read_batch: 500,
            max_entities: 4096,
            limits: Limits::default(),
        }
    }

    /// Reject settings that cannot work.
    ///
    /// # Errors
    /// `InvalidConfig` for an empty tenant or source, a zero bound or unusable closure limits.
    pub fn validate(&self) -> Result<(), HubError> {
        if self.tenant.is_empty() || self.source.is_empty() {
            return Err(HubError::InvalidConfig {
                reason: "the tenant and the source must not be empty",
            });
        }
        if self.max_queue_bytes == 0
            || self.max_bundles_per_tick_per_dest == 0
            || self.read_batch == 0
            || self.max_entities == 0
        {
            return Err(HubError::InvalidConfig {
                reason: "every bound must be above zero",
            });
        }
        self.limits.validate().map_err(HubError::from)
    }
}

/// What one destination got in one tick.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct DestReport {
    /// Bundles pushed (duplicates of an earlier push excluded).
    pub bundles_queued: usize,
    /// Bundles the queue already had (a replay after a restart).
    pub bundles_duplicate: usize,
    /// Entries withheld by a version-skew hold.
    pub held: usize,
    /// Entries withheld because a record their closure needs is held ("dependency held",
    /// `sync_hub.held_by_dependency`). They are released with the dependency on the upgrade.
    pub held_by_dependency: usize,
    /// Entries neither sent nor held because the hold set is full: the node is too old.
    pub unstored: usize,
    /// True when the destination is parked because its queue is full.
    pub backpressured: bool,
    /// True when the destination is behind the stream and will be caught up on later ticks.
    pub behind: bool,
    /// The catalogue code of the failure that parked this destination this tick, if any.
    pub failed: Option<&'static str>,
}

/// What one tick did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TickReport {
    /// The caller's clock for this tick, milliseconds.
    pub now_millis: i64,
    /// Entries read from the stream.
    pub read: usize,
    /// Entries dropped for every destination because of another tenant or source.
    pub foreign_dropped: usize,
    /// The highest position the relay has processed.
    pub cursor: u64,
    /// The catalogue code of a stream-contract violation this tick, if any: the read was refused
    /// before any bundle was built, nothing was queued and the cursor did not move. The same code
    /// is on every destination's own report.
    pub failed: Option<&'static str>,
    /// Per destination.
    pub destinations: BTreeMap<String, DestReport>,
    /// Destinations that sent a hostile or corrupt position since the last tick (an ack or a
    /// catch-up summary beyond the hub's own head), in id order. Raising the condition is
    /// WP-SYN-09.
    pub suspect: Vec<String>,
    /// Refused acks and summaries since the last tick (a counter, not a condition).
    pub suspect_events: u64,
    /// Audit events catch-up scoping dropped since the last tick (addressed to another node).
    pub audit_dropped: usize,
}

/// What `on_node_upgraded` did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReleaseReport {
    /// Entries released from holds by the new report.
    pub released: usize,
    /// Entries released because the record their closure needed is held no longer
    /// ("dependency held"): they wait for the dependency, so they leave with it.
    #[serde(default)]
    pub released_by_dependency: usize,
    /// Bundles requeued.
    pub bundles_queued: usize,
    /// Released entries not yet requeued (a retry runs on the next tick).
    pub pending: usize,
    /// Positions the hold set released that the relay had no entry for (lost state): the node's
    /// catch-up recovers them.
    pub missing: Vec<u64>,
}
