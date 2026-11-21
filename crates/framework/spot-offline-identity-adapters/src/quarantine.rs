//! The quarantine adapter: an offline write the hub cannot vouch for is held, never dropped.
//!
//! [`OfflineQuarantine`] implements the core crate's
//! [`QuarantinePort`](spot_offline_identity::QuarantinePort) over the item model of
//! `spot-quarantine` (WP-EDG-06.T2): the queue's own [`ItemId`], its [`State`] and
//! [`RecordRef`], and the reason code of the offline catalogue. The admin side reads the same
//! [`QuarantineItem`](spot_quarantine::QuarantineItem) values and builds its
//! [`ItemView`](spot_quarantine::ItemView) from them, so an offline write appears where a
//! person already looks for quarantined work.
//!
//! # Idempotent on `(node, journal_seq)`
//!
//! A refused acknowledgement makes the node send its journal again, so the same position is
//! offered more than once. The queue is keyed by `(node, journal_seq)`:
//!
//! - the same position with the **same** stamp stores nothing new and returns `Ok`
//!   ([`Placement::AlreadyHeld`]);
//! - the same position with a **different** stamp is [`AdapterError::QuarantineConflict`]. A
//!   held payload is immutable: the queue never replaces it, and a person looks at both.
//!
//! The reason is metadata, not payload: a replay after a later revocation is still the same
//! write, so it stays `Ok` and keeps the reason it was first held with.
//!
//! # Why an envelope, and not `QuarantineStore`
//!
//! `spot_quarantine::QuarantineStore::insert` takes a `ReceivedRecord` — the replica apply
//! path's record image. An offline stamp is not a record and is never applied, so fabricating
//! a document for it would put a write into the apply path that cannot be applied. The queue's
//! *item* model is reused here, and its immutable payload — the stamp — is stored beside it.
//!
//! # Time
//!
//! `first_seen` is supplied by the caller ([`place`](OfflineQuarantine::place) or
//! [`set_now`](OfflineQuarantine::set_now)); this crate never reads a clock.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use spot_offline_identity::{
    NodeId, OfflineError, OfflineStamp, QuarantineItem, QuarantinePort, QuarantineReason,
};
use spot_quarantine::{ItemId, QuarantineItem as QueueItem, RecordRef, State};
use spot_sync_protocol::{RecordName, SourceId};

use crate::error::AdapterError;

/// The entity name the queue shows for an offline write.
pub const OFFLINE_WRITE: &str = "offline_write";

/// Where queue envelopes live. `append` must be durable when it returns.
pub trait QuarantineEnvelopes {
    /// Every stored envelope, oldest first.
    ///
    /// # Errors
    /// [`AdapterError::QuarantineRead`] when the store cannot read.
    fn load(&self) -> Result<Vec<Vec<u8>>, AdapterError>;

    /// Appends one envelope.
    ///
    /// # Errors
    /// [`AdapterError::QuarantineWrite`] when the envelope is not durably stored.
    fn append(&mut self, envelope: &[u8]) -> Result<(), AdapterError>;
}

/// An in-memory [`QuarantineEnvelopes`], for tests and single-process use.
#[derive(Clone, Debug, Default)]
pub struct MemoryEnvelopes {
    /// The stored envelopes, oldest first.
    pub envelopes: Vec<Vec<u8>>,
    /// When set, every append fails.
    pub fail: bool,
}

impl QuarantineEnvelopes for MemoryEnvelopes {
    fn load(&self) -> Result<Vec<Vec<u8>>, AdapterError> {
        Ok(self.envelopes.clone())
    }

    fn append(&mut self, envelope: &[u8]) -> Result<(), AdapterError> {
        if self.fail {
            return Err(AdapterError::QuarantineWrite);
        }
        self.envelopes.push(envelope.to_vec());
        Ok(())
    }
}

/// One stored item: the queue's metadata and its immutable payload, the stamp.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Envelope {
    item: QueueItem,
    stamp: Option<OfflineStamp>,
}

/// What a placement did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// A new item was stored.
    Stored(QueueItem),
    /// The same position and the same payload were already held; nothing was stored.
    AlreadyHeld(QueueItem),
}

/// The quarantine queue of one hub, keyed by `(node, journal_seq)`.
pub struct OfflineQuarantine<S: QuarantineEnvelopes> {
    store: S,
    envelopes: Vec<Envelope>,
    index: BTreeMap<(String, u64), usize>,
    now_micros: i64,
}

impl<S: QuarantineEnvelopes> OfflineQuarantine<S> {
    /// Opens the queue over `store`, restoring and indexing every held item.
    ///
    /// # Errors
    /// [`AdapterError::QuarantineRead`] when the store cannot read, and
    /// [`AdapterError::QuarantineCorrupt`] when a stored envelope is not readable or two
    /// envelopes share a position. A queue that cannot be rebuilt is not used, because a
    /// missing index would silently store a duplicate.
    pub fn open(store: S) -> Result<Self, AdapterError> {
        let mut envelopes = Vec::new();
        let mut index = BTreeMap::new();
        for raw in store.load()? {
            let envelope: Envelope =
                serde_json::from_slice(&raw).map_err(|_| AdapterError::QuarantineCorrupt)?;
            let key = (envelope.item.source.as_str().to_owned(), envelope.item.seq);
            if index.insert(key, envelopes.len()).is_some() {
                return Err(AdapterError::QuarantineCorrupt);
            }
            envelopes.push(envelope);
        }
        Ok(Self {
            store,
            envelopes,
            index,
            now_micros: 0,
        })
    }

    /// The audit time the [`QuarantinePort`] implementation records as `first_seen`.
    pub fn set_now(&mut self, at_micros: i64) {
        self.now_micros = at_micros;
    }

    /// Every held item, in insertion order.
    pub fn items(&self) -> impl Iterator<Item = &QueueItem> {
        self.envelopes.iter().map(|e| &e.item)
    }

    /// How many items are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.envelopes.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.envelopes.is_empty()
    }

    /// The item held for `node` at `journal_seq`, if any.
    #[must_use]
    pub fn held(&self, node: &NodeId, journal_seq: u64) -> Option<&QueueItem> {
        self.position(node, journal_seq)
            .and_then(|p| self.envelopes.get(p))
            .map(|e| &e.item)
    }

    /// The stamp held for `node` at `journal_seq`: the item's immutable payload.
    #[must_use]
    pub fn held_stamp(&self, node: &NodeId, journal_seq: u64) -> Option<&OfflineStamp> {
        self.position(node, journal_seq)
            .and_then(|p| self.envelopes.get(p))
            .and_then(|e| e.stamp.as_ref())
    }

    /// The store (tests and the owner read it).
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The store (tests and the owner read it).
    #[must_use]
    pub fn into_store(self) -> S {
        self.store
    }

    fn position(&self, node: &NodeId, journal_seq: u64) -> Option<usize> {
        self.index
            .get(&(node.as_str().to_owned(), journal_seq))
            .copied()
    }

    /// Places one item at `at_micros`.
    ///
    /// # Errors
    /// [`AdapterError::QuarantineConflict`] when the position already holds another payload;
    /// [`AdapterError::QuarantineWrite`] when the store refuses the envelope;
    /// [`AdapterError::ItemId`] when the node id cannot key a queue item.
    pub fn place(
        &mut self,
        item: &QuarantineItem,
        at_micros: i64,
    ) -> Result<Placement, AdapterError> {
        if let Some(position) = self.position(&item.node, item.journal_seq) {
            let held = self
                .envelopes
                .get(position)
                .ok_or(AdapterError::QuarantineCorrupt)?;
            if held.stamp == item.stamp {
                return Ok(Placement::AlreadyHeld(held.item.clone()));
            }
            return Err(AdapterError::QuarantineConflict {
                node: item.node.as_str().to_owned(),
                journal_seq: item.journal_seq,
            });
        }
        let envelope = Envelope {
            item: to_queue_item(item, self.envelopes.len().saturating_add(1), at_micros)?,
            stamp: item.stamp.clone(),
        };
        let raw = serde_json::to_vec(&envelope).map_err(|_| AdapterError::QuarantineCorrupt)?;
        self.store.append(&raw)?;
        self.index.insert(
            (item.node.as_str().to_owned(), item.journal_seq),
            self.envelopes.len(),
        );
        self.envelopes.push(envelope.clone());
        Ok(Placement::Stored(envelope.item))
    }
}

impl<S: QuarantineEnvelopes> fmt::Debug for OfflineQuarantine<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfflineQuarantine")
            .field("items", &self.envelopes.len())
            .finish_non_exhaustive()
    }
}

impl<S: QuarantineEnvelopes> QuarantinePort for OfflineQuarantine<S> {
    fn quarantine(&mut self, item: &QuarantineItem) -> Result<(), OfflineError> {
        match self.place(item, self.now_micros) {
            Ok(_) => Ok(()),
            Err(AdapterError::QuarantineConflict { node, journal_seq }) => {
                Err(OfflineError::QuarantineConflict { node, journal_seq })
            }
            Err(_) => Err(OfflineError::QuarantineStore),
        }
    }
}

/// The queue's metadata for one offline write.
///
/// The write has no document, so its apply key is its journal position: `record_ref.version`
/// is the position and `record_ref.id` is the same position as text. The payload itself is the
/// stamp the envelope keeps beside it.
fn to_queue_item(
    item: &QuarantineItem,
    id: usize,
    at_micros: i64,
) -> Result<QueueItem, AdapterError> {
    let node = item.node.as_str();
    let source = SourceId::new(node).map_err(|_| AdapterError::ItemId)?;
    let name =
        RecordName::new(&format!("{OFFLINE_WRITE}:{node}")).map_err(|_| AdapterError::ItemId)?;
    Ok(QueueItem {
        id: ItemId(u64::try_from(id).map_err(|_| AdapterError::ItemId)?),
        source,
        seq: item.journal_seq,
        record_ref: RecordRef {
            name,
            version: item.journal_seq.max(1),
            entity: OFFLINE_WRITE.to_owned(),
            id: item.journal_seq.to_string(),
        },
        reason_code: item.reason.code().to_owned(),
        detail: detail(item.reason),
        first_seen: at_micros,
        attempts: 0,
        state: State::Held,
    })
}

/// A plain phrase for a person, one per reason. The admin side shows it beside the item.
fn detail(reason: QuarantineReason) -> String {
    match reason {
        QuarantineReason::TenantMismatch => "the stamp names another tenant",
        QuarantineReason::NodeMismatch => "the stamp names another node",
        QuarantineReason::SenderSuspended => "the sending node is suspended or decommissioned",
        QuarantineReason::SequenceReplay => "the device sequence repeats or goes backwards",
        QuarantineReason::UnknownEpoch => "the snapshot epoch was never issued to this node",
        QuarantineReason::AssignmentMismatch => "the assignment is not one of the author's",
        QuarantineReason::AuthorRevoked => "the author or the assignment is revoked",
        QuarantineReason::Tier1AfterRevocation => "a payments-tier action by a revoked author",
        QuarantineReason::BeyondBound => "the snapshot was older than the tier's bound",
        QuarantineReason::JournalTampered => "the journal's hash chain fails",
    }
    .to_owned()
}
