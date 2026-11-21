//! One quarantined item: its metadata, its state, and the immutable reference to its payload.
//!
//! A [`QuarantineItem`] is the queue's own record of a received item the apply path refused (or
//! that was parked behind one). It carries the **reason** the apply path gave, the sender and
//! stream position it came from, and a [`RecordRef`] that names the record. The record image
//! itself is held by the [`store`](crate::store), never here, and is never edited or deleted
//! (WP-EDG-06.T2 "must not").

use serde::{Deserialize, Serialize};
use spot_replica_apply::ReceivedRecord;
use spot_sync_protocol::{RecordName, SourceId};

use crate::error::QuarantineError;

/// The largest record image (its JSON text) the queue stores, in bytes: 256 KiB. A larger one is
/// refused with `quarantine.too_large`, and the intake keeps a short marker item instead, so one
/// hostile peer cannot fill the table.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
/// The longest `detail` an item may carry, in bytes: 4 KiB.
pub const MAX_DETAIL_BYTES: usize = 4 * 1024;
/// The longest reason code an item may carry, in bytes.
pub const MAX_REASON_CODE_BYTES: usize = 128;
/// The longest source (sender) name an item may carry, in bytes.
pub const MAX_SOURCE_BYTES: usize = 128;

/// The queue's own id for one item, assigned by the store in insertion order.
///
/// It is deliberately not a [`RecordId`](spot_storage_contract::RecordId): an item id is a queue
/// position, not a document key, and the store assigns it so the queue survives a restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(pub u64);

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where a quarantined item is in its life (ADR-062 G6, ADR-067 `QUARANTINED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Stored, waiting for a retry (the initial state, and where a failed retry returns).
    Held,
    /// An automatic retry is under way (the item is re-applied now).
    Retrying,
    /// Re-applied successfully and closed; the payload is kept for the record.
    Released,
    /// An operator discarded it; the payload is kept and the decision is audited.
    Discarded,
}

impl State {
    /// Whether the item can still be retried or released.
    #[must_use]
    pub fn is_open(self) -> bool {
        matches!(self, State::Held | State::Retrying)
    }

    /// Whether an item in this state may move to `next`: `Held` to `Retrying` and back, and
    /// `Held` or `Retrying` to `Released` or `Discarded`. A closed state is **terminal**: nothing
    /// follows `Released` or `Discarded`, not even the same state again.
    #[must_use]
    pub fn can_become(self, next: State) -> bool {
        match self {
            State::Held => matches!(next, State::Retrying | State::Released | State::Discarded),
            State::Retrying => matches!(next, State::Held | State::Released | State::Discarded),
            State::Released | State::Discarded => false,
        }
    }

    /// The stable text form the admin views and messages use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            State::Held => "held",
            State::Retrying => "retrying",
            State::Released => "released",
            State::Discarded => "discarded",
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a quarantined item is about: the record's apply key and the document it carries.
///
/// `entity` and `id` are text (the storage names and the UUIDv7 text form), so the whole ref is
/// serialisable and a persistent store can keep it as-is. The typed values are in the record
/// image the store holds beside the item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordRef {
    /// The apply key's name part (the record name, `entity:id` for a document version).
    pub name: RecordName,
    /// The apply key's version part; at least 1.
    pub version: u64,
    /// The root document's entity, as a storage name.
    pub entity: String,
    /// The root document's record id, as UUIDv7 text.
    pub id: String,
}

impl RecordRef {
    /// The ref of a received record.
    #[must_use]
    pub fn of(record: &ReceivedRecord) -> Self {
        Self {
            name: record.name.clone(),
            version: record.version,
            entity: record.document.entity.as_str().to_owned(),
            id: record.document.id.to_string(),
        }
    }

    /// The apply key as text (`name@version`).
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// One quarantined item: the queue's metadata for a refused or parked record.
///
/// The brief's fields, exactly: `id`, `source`, `seq`, `record_ref`, `reason_code`, `detail`,
/// `first_seen`, `attempts`, `state`. The record image is held by the store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineItem {
    /// The queue's id, assigned by the store.
    pub id: ItemId,
    /// The node the record came from.
    pub source: SourceId,
    /// The record's position in its source's stream.
    pub seq: u64,
    /// What the item is about.
    pub record_ref: RecordRef,
    /// Why it is quarantined: a `replica_apply.*` rejection code, or
    /// [`codes::DEPENDENT_HELD`](crate::codes::DEPENDENT_HELD) for a parked dependent.
    pub reason_code: String,
    /// The apply path's own explanation, or the item this one waits behind.
    pub detail: String,
    /// Unix microseconds the item was first seen; supplied by the caller, never read from a clock.
    pub first_seen: i64,
    /// How many automatic retries have run (the cap counts this).
    pub attempts: u32,
    /// Where the item is in its life.
    pub state: State,
}

impl QuarantineItem {
    /// The text an audit entry and a condition use for this item.
    #[must_use]
    pub fn subject(&self) -> String {
        self.record_ref.key()
    }
}

/// How an open item is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureOutcome {
    /// Re-applied successfully.
    Released,
    /// An operator discarded it.
    Discarded,
}

impl ClosureOutcome {
    /// The state the outcome leaves the item in.
    #[must_use]
    pub fn state(self) -> State {
        match self {
            ClosureOutcome::Released => State::Released,
            ClosureOutcome::Discarded => State::Discarded,
        }
    }
}

/// The record of a closure: how, by whom and when. It is written once with the closing state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closure {
    /// How the item is closed.
    pub outcome: ClosureOutcome,
    /// Who closes it: the user's name, or `system` for the automatic retry.
    pub by: String,
    /// Unix microseconds, supplied by the caller.
    pub at: i64,
}

/// Who closed an item and when, as the store holds it
/// ([`QuarantineStore::closure`](crate::store::QuarantineStore::closure)). Set once by
/// [`close`](crate::store::QuarantineStore::close) and never changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosureRecord {
    /// Who closed the item: the user's name, or `system` for the automatic retry.
    pub by: String,
    /// Unix microseconds the item was closed.
    pub at: i64,
}

/// A new item before the store assigns its [`ItemId`].
///
/// A store owns id allocation so the queue keeps its order across a restart; the intake builds a
/// `NewItem` and the store returns the stored [`QuarantineItem`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewItem {
    /// The node the record came from.
    pub source: SourceId,
    /// The record's position in its source's stream.
    pub seq: u64,
    /// What the item is about.
    pub record_ref: RecordRef,
    /// The rejection code (a `replica_apply.*` code, or `quarantine.dependent_held`).
    pub reason_code: String,
    /// The apply path's explanation, or what it waits behind.
    pub detail: String,
    /// Unix microseconds, supplied by the caller.
    pub first_seen: i64,
}

impl NewItem {
    /// Checks the size limits that apply to the metadata of a new item.
    ///
    /// # Errors
    /// [`QuarantineError::TooLarge`] for the first value over its limit; nothing is truncated.
    pub fn check_sizes(&self) -> Result<(), QuarantineError> {
        for (what, limit, actual) in [
            ("source", MAX_SOURCE_BYTES, self.source.as_str().len()),
            ("reason code", MAX_REASON_CODE_BYTES, self.reason_code.len()),
            ("detail", MAX_DETAIL_BYTES, self.detail.len()),
        ] {
            if actual > limit {
                return Err(QuarantineError::TooLarge {
                    what,
                    limit,
                    actual,
                });
            }
        }
        Ok(())
    }

    /// A held item with no attempts yet.
    #[must_use]
    pub fn held(
        source: SourceId,
        seq: u64,
        record_ref: RecordRef,
        reason_code: impl Into<String>,
        detail: impl Into<String>,
        first_seen: i64,
    ) -> Self {
        Self {
            source,
            seq,
            record_ref,
            reason_code: reason_code.into(),
            detail: detail.into(),
            first_seen,
        }
    }
}

/// Which items a [`list`](crate::store::QuarantineStore::list) returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemFilter {
    /// Only items in this state, when set.
    pub state: Option<State>,
    /// Only items with this reason code, when set.
    pub reason_code: Option<String>,
    /// Only items from this sender, when set.
    pub source: Option<SourceId>,
}

impl ItemFilter {
    /// Every item.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// Whether `item` passes the filter.
    #[must_use]
    pub fn matches(&self, item: &QuarantineItem) -> bool {
        if let Some(state) = self.state
            && item.state != state
        {
            return false;
        }
        if let Some(reason) = &self.reason_code
            && &item.reason_code != reason
        {
            return false;
        }
        if let Some(source) = &self.source
            && &item.source != source
        {
            return false;
        }
        true
    }
}

/// The size of a record image as the store keeps it, or an error if it cannot be encoded.
///
/// # Errors
/// [`QuarantineError::Store`] when the record cannot be serialised.
pub fn payload_len(record: &ReceivedRecord) -> Result<usize, QuarantineError> {
    record
        .payload_json()
        .map(|text| text.len())
        .map_err(|_| QuarantineError::Store {
            why: "the record image could not be serialised".to_owned(),
        })
}

/// Refuses a record image over [`MAX_PAYLOAD_BYTES`].
///
/// # Errors
/// [`QuarantineError::TooLarge`] when the image is too large, [`QuarantineError::Store`] when it
/// cannot be serialised.
pub fn check_payload(record: &ReceivedRecord) -> Result<(), QuarantineError> {
    let actual = payload_len(record)?;
    if actual > MAX_PAYLOAD_BYTES {
        return Err(QuarantineError::TooLarge {
            what: "payload",
            limit: MAX_PAYLOAD_BYTES,
            actual,
        });
    }
    Ok(())
}
