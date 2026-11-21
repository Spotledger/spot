//! Documents: a parent record and its child rows, stored and loaded as one unit (ADR-33).

use std::collections::BTreeMap;
use std::future::Future;

use crate::error::StorageResult;
use crate::id::{Name, RecordId, Version};
use crate::locks::RowLock;
use crate::value::Value;

/// Field values by field name.
pub type Fields = BTreeMap<Name, Value>;

/// A document as written: parent fields and child tables, without a version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentData {
    /// The document entity.
    pub entity: Name,
    /// The document's id, chosen by the caller (normally [`RecordId::new_v7`]).
    pub id: RecordId,
    /// Every declared field of the entity, and nothing else.
    pub fields: Fields,
    /// Child rows per child-table field, in order. A missing table means no rows.
    pub children: BTreeMap<Name, Vec<ChildRow>>,
}

/// One row of a child table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildRow {
    /// The row's own id (ADR-33); line-level references point here.
    pub id: RecordId,
    /// Every declared field of the child entity, and nothing else.
    pub fields: Fields,
}

/// A stored document with the version it was read at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    /// The content. Child rows come back in their stored order.
    pub data: DocumentData,
    /// The stored version.
    pub version: Version,
}

/// Document operations, available inside a [`Transaction`](crate::Transaction).
///
/// Rules every backend follows:
/// - Writes are full: `fields` must contain exactly the entity's declared fields, each
///   passing [`FieldType::check`](crate::FieldType::check); otherwise
///   [`ErrorKind::InvalidValue`](crate::ErrorKind::InvalidValue) names the field.
/// - Child rows are stored as separate records with the system columns `parent`,
///   `parent_field` and `idx` (1-based order in the list).
/// - `update` replaces the child tables: rows whose id is still present are updated in place,
///   new ids are inserted, missing ids are deleted, and `idx` follows the new order.
/// - A child row id that belongs to another document fails with `AlreadyExists`.
/// - Links with [`LinkCheck::Database`](crate::LinkCheck) (and every child row's `parent`) must
///   point at existing records when the transaction commits. Checks are deferred to commit, so
///   documents may be saved in any order and may link to each other in a cycle within one
///   transaction. A link to a missing record, deleting a document, or removing a child row
///   through `update` while such a link still points at it fails with `ReferenceViolation`,
///   from the write or at the latest from `commit`. Storage never cascades a delete to linking
///   records. `EngineOnly` and dynamic links are not checked by storage; the document engine
///   checks them and owns what every link means (WP-DOC-06).
/// - Every write of a document is reported on the [`ChangeFeed`](crate::ChangeFeed) after commit.
/// - Documents of child entities cannot be written directly (`UnknownEntity` explains why).
pub trait DocumentStore: Send {
    /// Inserts a new document with its child rows. Returns [`Version::FIRST`].
    /// An existing id fails with `AlreadyExists`.
    fn insert(&mut self, doc: &DocumentData)
    -> impl Future<Output = StorageResult<Version>> + Send;

    /// Loads a document and its child rows, or `None` if it does not exist. With a lock, the
    /// parent row stays locked until the transaction ends (see [`RowLock`]).
    fn load(
        &mut self,
        entity: &Name,
        id: RecordId,
        lock: RowLock,
    ) -> impl Future<Output = StorageResult<Option<Document>>> + Send;

    /// Replaces a stored document. Fails with `StaleVersion` unless the stored version equals
    /// `expected`, and with `NotFound` if it does not exist. Returns the new version.
    fn update(
        &mut self,
        doc: &DocumentData,
        expected: Version,
    ) -> impl Future<Output = StorageResult<Version>> + Send;

    /// Deletes a document and all its child rows, under the same version rule as `update`.
    fn delete(
        &mut self,
        entity: &Name,
        id: RecordId,
        expected: Version,
    ) -> impl Future<Output = StorageResult<()>> + Send;
}
