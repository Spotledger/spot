//! Records, committed data, a transaction's write overlay and the merged read view.

use std::collections::BTreeMap;
use std::iter::Peekable;

use spot_storage_contract::schema::system;
use spot_storage_contract::{Fields, Name, NodeRef, RecordId, Value, Version};

/// One stored record: a document's parent row or a child row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    /// Declared field values.
    pub fields: Fields,
    /// Document or child bookkeeping.
    pub kind: RecordKind,
}

/// The system columns of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// A document's parent row.
    Document {
        /// The document version.
        version: Version,
        /// Child row ids per child-table field, in `idx` order.
        children: BTreeMap<Name, Vec<RecordId>>,
    },
    /// A child row.
    Child {
        /// The parent document's id.
        parent: RecordId,
        /// The parent's child-table field.
        parent_field: Name,
        /// 1-based position in that table.
        idx: u32,
    },
}

impl Record {
    /// The value of a declared field or system column, or `None` if the record has neither.
    pub fn value(&self, id: RecordId, field: &Name) -> Option<Value> {
        match (field.as_str(), &self.kind) {
            (system::ID, _) => Some(Value::Id(id)),
            (system::VERSION, RecordKind::Document { version, .. }) => {
                Some(Value::Int(i64::try_from(version.0).unwrap_or(i64::MAX)))
            }
            (system::PARENT, RecordKind::Child { parent, .. }) => Some(Value::Id(*parent)),
            (system::PARENT_FIELD, RecordKind::Child { parent_field, .. }) => {
                Some(Value::Text(parent_field.as_str().to_owned()))
            }
            (system::IDX, RecordKind::Child { idx, .. }) => Some(Value::Int(i64::from(*idx))),
            _ => self.fields.get(field).cloned(),
        }
    }

    /// The document version, for a parent row.
    pub fn version(&self) -> Option<Version> {
        match &self.kind {
            RecordKind::Document { version, .. } => Some(*version),
            RecordKind::Child { .. } => None,
        }
    }

    /// The parent document id, for a child row.
    pub fn parent(&self) -> Option<RecordId> {
        match &self.kind {
            RecordKind::Child { parent, .. } => Some(*parent),
            RecordKind::Document { .. } => None,
        }
    }

    /// Every child row id of a parent row.
    pub fn child_ids(&self) -> impl Iterator<Item = (&Name, RecordId)> {
        let children = match &self.kind {
            RecordKind::Document { children, .. } => Some(children),
            RecordKind::Child { .. } => None,
        };
        children.into_iter().flat_map(|c| {
            c.iter()
                .flat_map(|(f, ids)| ids.iter().map(move |id| (f, *id)))
        })
    }
}

/// Records of one entity, by id.
pub(crate) type Table = BTreeMap<RecordId, Record>;

static EMPTY_TABLE: Table = BTreeMap::new();
static EMPTY_OVERLAY_TABLE: BTreeMap<RecordId, Option<Record>> = BTreeMap::new();

/// Committed records of every entity.
#[derive(Clone, Debug, Default)]
pub(crate) struct Data {
    tables: BTreeMap<Name, Table>,
}

impl Data {
    /// The committed record, if any.
    pub fn get(&self, entity: &Name, id: RecordId) -> Option<&Record> {
        self.tables.get(entity).and_then(|t| t.get(&id))
    }

    /// The table of `entity`; empty if nothing was ever stored.
    pub fn table(&self, entity: &Name) -> &Table {
        self.tables.get(entity).unwrap_or(&EMPTY_TABLE)
    }

    /// Mutable table of `entity`.
    pub fn table_mut(&mut self, entity: &Name) -> &mut Table {
        self.tables.entry(entity.clone()).or_default()
    }

    /// Whether `entity` holds any record.
    pub fn has_rows(&self, entity: &Name) -> bool {
        !self.table(entity).is_empty()
    }
}

/// What a transaction did to one document, for version re-checks and the change feed.
#[derive(Clone, Debug)]
pub(crate) struct DocOp {
    /// Order of the first write in the transaction.
    pub order: u64,
    /// The committed version when the transaction first wrote the document (`None`: absent).
    pub base: Option<Version>,
    /// Whether the document is deleted at the end of the transaction.
    pub deleted: bool,
    /// Final version, or the last version before deletion.
    pub version: Version,
}

/// A transaction's private writes. `None` marks a deleted record.
#[derive(Clone, Debug, Default)]
pub(crate) struct Overlay {
    /// Written records per entity.
    pub records: BTreeMap<Name, BTreeMap<RecordId, Option<Record>>>,
    /// Written documents.
    pub docs: BTreeMap<NodeRef, DocOp>,
    /// Records inserted, updated or deleted, plus outbox events (the `max_writes` count).
    pub writes: u32,
}

impl Overlay {
    /// The overlay entry for a record: `Some(None)` if deleted, `None` if untouched.
    pub fn entry(&self, entity: &Name, id: RecordId) -> Option<Option<&Record>> {
        self.records
            .get(entity)
            .and_then(|t| t.get(&id))
            .map(Option::as_ref)
    }

    /// Writes (or deletes, with `None`) a record, returning the previous overlay entry.
    pub fn put(
        &mut self,
        entity: &Name,
        id: RecordId,
        record: Option<Record>,
    ) -> Option<Option<Record>> {
        self.records
            .entry(entity.clone())
            .or_default()
            .insert(id, record)
    }

    /// Puts back an entry returned by [`Overlay::put`].
    pub fn restore(&mut self, entity: &Name, id: RecordId, previous: Option<Option<Record>>) {
        let table = self.records.entry(entity.clone()).or_default();
        match previous {
            Some(p) => {
                table.insert(id, p);
            }
            None => {
                table.remove(&id);
            }
        }
    }

    /// Whether the transaction wrote anything.
    pub fn is_empty(&self) -> bool {
        self.records.values().all(BTreeMap::is_empty)
    }
}

/// Committed data merged with a transaction's overlay: what the transaction reads.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
    base: &'a Data,
    overlay: Option<&'a Overlay>,
}

impl<'a> View<'a> {
    /// Committed data merged with `overlay`.
    pub fn new(base: &'a Data, overlay: &'a Overlay) -> Self {
        Self {
            base,
            overlay: Some(overlay),
        }
    }

    /// Committed data only (reads outside a transaction).
    pub fn committed(base: &'a Data) -> Self {
        Self {
            base,
            overlay: None,
        }
    }

    /// The visible record, if any.
    pub fn get(&self, entity: &Name, id: RecordId) -> Option<&'a Record> {
        match self.overlay.and_then(|o| o.entry(entity, id)) {
            Some(written) => written,
            None => self.base.get(entity, id),
        }
    }

    /// Every visible record of `entity`, in id order.
    pub fn rows(&self, entity: &Name) -> impl Iterator<Item = (RecordId, &'a Record)> + 'a {
        let written = self
            .overlay
            .and_then(|o| o.records.get(entity))
            .unwrap_or(&EMPTY_OVERLAY_TABLE);
        Merge {
            base: self.base.table(entity).iter().peekable(),
            written: written.iter().peekable(),
        }
    }
}

/// Merges a committed table with overlay entries, both in id order.
struct Merge<B: Iterator, W: Iterator> {
    base: Peekable<B>,
    written: Peekable<W>,
}

impl<'a, B, W> Iterator for Merge<B, W>
where
    B: Iterator<Item = (&'a RecordId, &'a Record)>,
    W: Iterator<Item = (&'a RecordId, &'a Option<Record>)>,
{
    type Item = (RecordId, &'a Record);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let next_base = self.base.peek().map(|(id, _)| **id);
            let next_written = self.written.peek().map(|(id, _)| **id);
            let take_base = match (next_base, next_written) {
                (None, None) => return None,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(b), Some(w)) => {
                    // An overlay entry replaces the committed record with the same id.
                    if b == w {
                        self.base.next();
                    }
                    b < w
                }
            };
            if take_base {
                return self.base.next().map(|(id, r)| (*id, r));
            }
            if let Some((id, Some(r))) = self.written.next() {
                return Some((*id, r));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Name {
        Name::new(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn rec(v: i64) -> Record {
        let mut fields = Fields::new();
        fields.insert(n("v"), Value::Int(v));
        Record {
            fields,
            kind: RecordKind::Document {
                version: Version::FIRST,
                children: BTreeMap::new(),
            },
        }
    }

    #[test]
    fn view_merges_overlay_in_id_order() {
        let e = n("thing");
        let ids: Vec<RecordId> = (0..4).map(|_| RecordId::new_v7()).collect();
        let mut data = Data::default();
        data.table_mut(&e).insert(ids[0], rec(0));
        data.table_mut(&e).insert(ids[2], rec(2));
        data.table_mut(&e).insert(ids[3], rec(3));
        let mut overlay = Overlay::default();
        overlay.put(&e, ids[1], Some(rec(1)));
        overlay.put(&e, ids[2], None);
        overlay.put(&e, ids[3], Some(rec(30)));
        let view = View::new(&data, &overlay);
        let got: Vec<(RecordId, Option<Value>)> = view
            .rows(&e)
            .map(|(id, r)| (id, r.value(id, &n("v"))))
            .collect();
        assert_eq!(
            got,
            vec![
                (ids[0], Some(Value::Int(0))),
                (ids[1], Some(Value::Int(1))),
                (ids[3], Some(Value::Int(30))),
            ]
        );
        assert!(view.get(&e, ids[2]).is_none());
        assert_eq!(View::committed(&data).rows(&e).count(), 3);
    }
}
