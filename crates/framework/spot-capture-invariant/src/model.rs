//! The plain summaries the checker compares. They mirror the field names of the storage change
//! feed (ADR-053) and the journal's `doc_state` event (ADR-061) without depending on either
//! contract: reading the real feed and events is the document engine's wiring task.

use serde::{Deserialize, Serialize};

/// A place in a stream: the generation (a fork or restore starts a new one) and the sequence
/// number within it. Ordered by generation, then sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position {
    /// The stream generation.
    pub generation: u64,
    /// The sequence number within the generation.
    pub seq: u64,
}

/// What a transaction did to a document, taken as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Created (possibly also updated) in the transaction.
    Insert,
    /// Updated.
    Update,
    /// Deleted (possibly after updates) in the transaction.
    Delete,
}

/// One entry of the storage change feed: the independent witness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedEntry {
    /// Position in the change feed.
    pub position: Position,
    /// The DocType (entity) name.
    pub entity: String,
    /// The document id.
    pub id: String,
    /// The committed version (for a delete, the last version before deletion).
    pub version: u64,
    /// The net operation of the transaction.
    pub op: Op,
}

/// One `doc_state` event of the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateEvent {
    /// Position in the journal.
    pub position: Position,
    /// The DocType (entity) name.
    pub entity: String,
    /// The document id.
    pub id: String,
    /// The version of the image the event carries.
    pub version: u64,
    /// The operation the event records.
    pub op: Op,
}
