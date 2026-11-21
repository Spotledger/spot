//! The transactional outbox (ADR-11) and the change feed (ADR-053, "Outbox and change feed").
//!
//! - The **outbox** holds business events written explicitly in a document's transaction.
//!   Consumers are the job engine, integrations, `*_committed` hooks (ADR-51) and branch sync
//!   (ADR-23).
//! - The **change feed** holds one entry per document per committed transaction, produced by the
//!   backend itself. Consumers are cache invalidation (ADR-20), the in-memory graph index
//!   (WP-GRA-02) and drift checks.
//!
//! Business transactions only *capture* entries; they never take a shared lock or counter for
//! the logs. Committed entries are then *published* into a feed that readers page through with a
//! [`FeedPosition`] bookmark. Positions are **publication order**, not commit order. Guarantees
//! for both logs:
//!
//! 1. **Atomic capture.** An entry exists only if its transaction commits.
//! 2. **Prompt visibility.** Reading publishes pending committed entries first, so an entry whose
//!    transaction committed before a read call started is returned by that read or an earlier
//!    one (within `limit`). A background publisher may publish sooner.
//! 3. **Safe bookmarks.** Positions strictly increase. Once a reader has seen position `p`, no
//!    entry at or before `p` can appear later, so a reader that stores its last position never
//!    misses an entry, even when a transaction that started earlier commits later.
//! 4. **Transaction order.** A transaction's outbox events are published in the order they were
//!    enqueued ([`OutboxRecord::ordinal`]).
//! 5. **Causal order.** An entry written after another transaction committed is published after
//!    all of that transaction's entries. So a customer created before the invoice that uses it,
//!    or a document revision written after the previous revision committed, is never published
//!    out of order. Entries of concurrent, unrelated transactions have no defined relative order.
//! 6. **Explicit recovery.** Entries are kept at least for
//!    [`Capabilities::feed_retention`](crate::Capabilities). A position that is too old, from
//!    another stream generation (for example after a restore), or beyond the end of the feed
//!    fails with `ResyncRequired`, never with an empty page. [`FeedPosition::START`] is always
//!    valid and reads the current generation from its beginning.
//!
//! Delivery is at least once: consumers deduplicate by event id (outbox) or by document id and
//! version (change feed) and keep their own inbox and bookmark. Envelope fields beyond these
//! (source, command and causation ids) and payload schemas are WP-CON-04.

use std::future::Future;

use crate::error::StorageResult;
use crate::graph::NodeRef;
use crate::id::{Name, RecordId, Version};

/// A bookmark in the outbox or the change feed. Each log, and each source database, has its own
/// positions: store and return them, never compare them across sources or do arithmetic on
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedPosition {
    generation: u64,
    seq: u64,
}

impl FeedPosition {
    /// Before the first entry of the current stream generation.
    pub const START: FeedPosition = FeedPosition {
        generation: 0,
        seq: 0,
    };

    /// A position; for backends and for consumers restoring a stored bookmark.
    pub fn new(generation: u64, seq: u64) -> Self {
        Self { generation, seq }
    }

    /// The stream generation. A backend starts a new generation whenever old positions lose
    /// continuity, for example after a restore from backup.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The sequence number within the generation.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

/// A business event to publish after commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxEvent {
    /// Unique, stable event id: the consumers' deduplication key. Enqueuing the same id twice
    /// fails with `AlreadyExists`.
    pub id: RecordId,
    /// Event type, for example `sales_invoice_posted`.
    pub topic: Name,
    /// The document the event is about; its ordering scope.
    pub subject: NodeRef,
    /// The subject's revision this event belongs to, when the event comes from a document write.
    /// Consumers use it to keep order per subject and to ignore stale events.
    pub subject_version: Option<Version>,
    /// Version of the payload schema (WP-CON-04).
    pub schema_version: u32,
    /// The payload as JSON text: immutable facts as of the event.
    pub payload: String,
}

/// A published outbox event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxRecord {
    /// Position in the outbox feed.
    pub position: FeedPosition,
    /// Id of the transaction that enqueued the event, assigned by the backend; groups the
    /// events of one business transaction.
    pub transaction: RecordId,
    /// Enqueue order within that transaction, from 0.
    pub ordinal: u32,
    /// The event.
    pub event: OutboxEvent,
}

/// What a transaction did to a document, taken as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeOp {
    /// Created (possibly also updated) in the transaction.
    Insert,
    /// Updated (parent fields, child rows or both).
    Update,
    /// Deleted (possibly after updates) in the transaction.
    Delete,
}

/// One document's net change in one committed transaction.
///
/// A transaction produces exactly one entry per document it wrote, carrying the final committed
/// version. A document both created and deleted in the same transaction produces none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Position in the change feed.
    pub position: FeedPosition,
    /// Id of the transaction, as in [`OutboxRecord::transaction`].
    pub transaction: RecordId,
    /// The document.
    pub document: NodeRef,
    /// The net operation.
    pub op: ChangeOp,
    /// The committed version; for a delete, the last version before deletion.
    pub version: Version,
}

/// Writing to the outbox, inside a transaction.
pub trait Outbox: Send {
    /// Captures an event, published only if the transaction commits.
    fn enqueue(&mut self, event: &OutboxEvent) -> impl Future<Output = StorageResult<()>> + Send;
}

/// Reading the outbox and the change feed, outside transactions.
pub trait ChangeFeed: Send + Sync {
    /// Publishes pending committed events, then returns up to `limit` published events after
    /// `after`, in position order.
    fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> impl Future<Output = StorageResult<Vec<OutboxRecord>>> + Send;

    /// Publishes pending committed changes, then returns up to `limit` changes after `after`, in
    /// position order.
    fn read_changes(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> impl Future<Output = StorageResult<Vec<Change>>> + Send;
}
