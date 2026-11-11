//! The seams: the stream, the send queue, the destination's scope and the ack observer.

use crate::error::HubError;
use crate::model::{PushOutcome, QueuedBundle, StreamEntry};
use spot_conditions::AuditSink;
use spot_sync_acks::{AckError, AckLedger, NodeReportRow, NodeStateStore, StreamHead};
use spot_sync_catchup::Event;
use spot_sync_closure::RecordKey;
use spot_sync_freshness::{MandatoryEvent, RejectNote};
use spot_sync_obligations::{MandatoryEvent as ObligationEvent, Pending};

/// The hub's stream, read for one source.
pub trait HubStream {
    /// Entries with a position greater than `after`, ascending, at most about `max` of them.
    ///
    /// Contract: **whole source transactions only**. When the `max` limit falls inside a
    /// transaction the adapter returns the rest of that transaction too, so a transaction is never
    /// split across two reads (a bundle is applied by the node in one transaction).
    ///
    /// # Errors
    /// `StreamFailed` when the adapter fails.
    fn read_after(&self, after: u64, max: usize) -> Result<Vec<StreamEntry>, HubError>;

    /// The newest position the hub's stream holds for this tenant (0 when it holds none).
    ///
    /// This is the hub's own value: the bound on every position a node reports. It is **not** the
    /// relay's read cursor, which trails it.
    ///
    /// # Errors
    /// `StreamFailed` when the adapter fails.
    fn head(&self) -> Result<u64, HubError>;
}

/// The events of the hub stream as the catch-up planner reads them, for a gap
/// `(after, through]`. The host adapts the journal or hot outbox; ascending positions.
pub trait GapSource {
    /// Events with `after < position <= through`.
    ///
    /// # Errors
    /// `StreamFailed` when the adapter fails.
    fn events_between(&self, after: u64, through: u64) -> Result<Vec<Event>, HubError>;
}

/// The hub's acknowledgement book: the node-state ledger the relay feeds.
///
/// Every method names the tenant. The relay passes **its own configured tenant** on every call
/// (`RelayConfig::tenant`), never a value a node sent, so a book shared by the relays of several
/// tenants keeps their obligations apart: the ledger keys everything by `(node, tenant)`
/// (WP-SYN-05.T5). The relay passes the head from its own tenant stream; the book never learns a
/// head from a node.
pub trait AckBook {
    /// Record that `node` applied `tenant`'s stream through `applied_prefix`, heard at hub time
    /// `at`, with `head` the newest position the hub holds for `tenant`.
    ///
    /// # Errors
    /// The ledger's refusal; nothing is recorded then.
    fn on_ack(
        &mut self,
        node: &str,
        tenant: &str,
        applied_prefix: u64,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError>;

    /// `node` reported at hub time `at` and applied nothing new, for `tenant` only.
    ///
    /// # Errors
    /// `UnknownNode` when the node is not registered for `tenant`.
    fn on_checkin(&mut self, node: &str, tenant: &str, at: i64) -> Result<(), AckError>;

    /// Replace the mandatory positions `node` holds back for `tenant`; `head` is that tenant's
    /// stream head.
    ///
    /// # Errors
    /// The ledger's refusal; nothing is recorded then.
    fn set_holds(
        &mut self,
        node: &str,
        tenant: &str,
        holds: Vec<u64>,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError>;

    /// Register `node` for `tenant`. Returns whether the pair was new.
    ///
    /// # Errors
    /// `IdInvalid` for an id the ledger cannot key.
    fn register_node(
        &mut self,
        node: &str,
        tenant: &str,
        version: &str,
        at: i64,
    ) -> Result<bool, AckError>;

    /// Register a mandatory event of `tenant`. Returns whether it was new.
    fn register_mandatory(&mut self, tenant: &str, event: ObligationEvent) -> bool;

    /// The mandatory events registered for `tenant`, in the freshness check's form.
    fn mandatory_events(&self, tenant: &str) -> Vec<MandatoryEvent>;

    /// The nodes of `tenant` that have not applied its event at `position`.
    ///
    /// # Errors
    /// `UnknownEvent` when `tenant` has no event there.
    fn who_is_missing(&self, tenant: &str, position: u64) -> Result<Vec<NodeReportRow>, AckError>;
}

/// A ledger as the relay's [`AckBook`]. It holds no tenant: the relay supplies its own on every
/// call. To share one ledger between the relays of several tenants, implement [`AckBook`] over a
/// shared handle to it (the book is a seam; the sharing mechanism is the host's).
pub struct LedgerBook<S: NodeStateStore, A: AuditSink> {
    ledger: AckLedger<S, A>,
}

impl<S: NodeStateStore, A: AuditSink> LedgerBook<S, A> {
    /// Wraps `ledger`.
    #[must_use]
    pub fn new(ledger: AckLedger<S, A>) -> Self {
        Self { ledger }
    }

    /// The ledger, for reading its state.
    #[must_use]
    pub fn ledger(&self) -> &AckLedger<S, A> {
        &self.ledger
    }

    /// The ledger, for registering nodes and events.
    pub fn ledger_mut(&mut self) -> &mut AckLedger<S, A> {
        &mut self.ledger
    }
}

impl<S: NodeStateStore, A: AuditSink> AckBook for LedgerBook<S, A> {
    fn on_ack(
        &mut self,
        node: &str,
        tenant: &str,
        applied_prefix: u64,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError> {
        self.ledger
            .on_ack(node, tenant, applied_prefix, head, at)
            .map(|_| ())
    }

    fn on_checkin(&mut self, node: &str, tenant: &str, at: i64) -> Result<(), AckError> {
        // The ledger's own check-in names no tenant and would bump the node's contact in every
        // tenant. A same-prefix ack is the per-tenant check-in: it touches this pair only and
        // takes no node-supplied position (the prefix is the ledger's own record).
        let Some(record) = self.ledger.store().load(node, tenant) else {
            return Err(AckError::UnknownNode {
                node: node.to_owned(),
                tenant: tenant.to_owned(),
            });
        };
        let prefix = record.applied_prefix;
        self.ledger
            .on_ack(node, tenant, prefix, StreamHead::new(prefix), at)
            .map(|_| ())
    }

    fn set_holds(
        &mut self,
        node: &str,
        tenant: &str,
        holds: Vec<u64>,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError> {
        self.ledger
            .set_holds(node, tenant, holds, head, at)
            .map(|_| ())
    }

    fn register_node(
        &mut self,
        node: &str,
        tenant: &str,
        version: &str,
        at: i64,
    ) -> Result<bool, AckError> {
        self.ledger.register_node(node, tenant, version, at)
    }

    fn register_mandatory(&mut self, tenant: &str, event: ObligationEvent) -> bool {
        self.ledger.register_mandatory(tenant, event)
    }

    fn mandatory_events(&self, tenant: &str) -> Vec<MandatoryEvent> {
        self.ledger
            .store()
            .events(tenant)
            .into_iter()
            .map(|e| MandatoryEvent {
                position: e.position,
                published_at: e.published_at_millis,
                effective_from: e.effective_from_millis,
                record: e.record,
            })
            .collect()
    }

    fn who_is_missing(&self, tenant: &str, position: u64) -> Result<Vec<NodeReportRow>, AckError> {
        self.ledger.who_is_missing(tenant, position)
    }
}

/// Receives the audit note for a stamp the hub refused. Notes carry node id and amounts only.
pub trait HubAudit {
    /// One refused stamp.
    fn stamp_rejected(&mut self, note: RejectNote);
}

/// An audit seam that drops notes.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAudit;

impl HubAudit for NoAudit {
    fn stamp_rejected(&mut self, _note: RejectNote) {}
}

/// The durable per-destination queue of bundles waiting to be sent.
///
/// Contract for a persistent implementation:
///
/// * `push` is idempotent on `(dest, item.position, item.item.index, item.item.redelivery)`: a
///   second push of the same identity returns [`PushOutcome::Duplicate`] and changes nothing. A
///   restart replays pushes, so this is what keeps the queue free of duplicates.
/// * `peek` returns at most `n` items in serving order: lane first (mandatory before bulk), then
///   position, then bundle index. Peeking removes nothing.
/// * `ack` removes every item with `position <= up_to` that is **not** a redelivery, and returns
///   how many it removed. A redelivery leaves only through `remove_redelivered`.
/// * `queued_bytes` is the sum of `bundle.bytes` of the items queued for `dest`.
/// * A pushed item is durable before `push` returns.
pub trait SendQueue {
    /// Add an item to a destination's queue.
    ///
    /// # Errors
    /// `QueueFailed` when the adapter fails.
    fn push(&mut self, dest: &str, item: Pending<QueuedBundle>) -> Result<PushOutcome, HubError>;

    /// The next `n` items in serving order.
    ///
    /// # Errors
    /// `QueueFailed` when the adapter fails.
    fn peek(&self, dest: &str, n: usize) -> Result<Vec<Pending<QueuedBundle>>, HubError>;

    /// Remove the acknowledged prefix. See the trait contract.
    ///
    /// # Errors
    /// `QueueFailed` when the adapter fails.
    fn ack(&mut self, dest: &str, up_to: u64) -> Result<usize, HubError>;

    /// Remove the redelivered items at `position`.
    ///
    /// # Errors
    /// `QueueFailed` when the adapter fails.
    fn remove_redelivered(&mut self, dest: &str, position: u64) -> Result<usize, HubError>;

    /// Bytes queued for a destination.
    ///
    /// # Errors
    /// `QueueFailed` when the adapter fails.
    fn queued_bytes(&self, dest: &str) -> Result<usize, HubError>;
}

/// What the hub knows of one destination's entitlement. Scope filtering is the access control of
/// sync: an implementation answers from the hub's registry (never from anything the node sent),
/// and answers `false` when in doubt about an *entry*.
pub trait DestinationScope {
    /// Whether the destination's subscription covers this entry (entity, scope keys, direction).
    fn covers(&self, entry: &StreamEntry) -> bool;
    /// Whether the record is inside the destination's scope (used by the closure to tell an
    /// in-scope dependency from an out-of-scope extra).
    fn covers_key(&self, key: &RecordKey) -> bool;
    /// Whether the record came from the destination.
    fn owns(&self, key: &RecordKey) -> bool;
}

/// Told when a destination's applied prefix advances. The obligation ledger update is
/// WP-SYN-05.T2; this is its seam.
pub trait AckObserver {
    /// The destination applied the stream through `applied_prefix`.
    fn on_ack(&mut self, dest: &str, applied_prefix: u64);
}

/// An observer that ignores acks.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoObserver;

impl AckObserver for NoObserver {
    fn on_ack(&mut self, _dest: &str, _applied_prefix: u64) {}
}
