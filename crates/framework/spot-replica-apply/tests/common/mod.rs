//! Shared fixtures for the apply-path tests: a small catalog, the in-memory backend, a record
//! builder and a couple of ownership implementations.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use spot_replica_apply::{ApplyCtx, ApplyOutcome, OwnerCheck, Ownership, Reason, ReceivedRecord};
use spot_storage_contract::{
    Catalog, ChildRow, ChildTableDef, Collation, Document, DocumentData, DocumentStore, EntityDef,
    EntityKind, FieldDef, FieldType, Fields, Isolation, Name, RecordId, RowLock, StorageBackend,
    Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;
use spot_sync_protocol::{RecordName, SourceId};

/// A validated storage name.
pub fn name(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn text() -> FieldType {
    FieldType::Text { max_chars: None }
}

fn decimal() -> FieldType {
    FieldType::Decimal {
        precision: 18,
        scale: 2,
    }
}

/// A nullable field of the given type.
fn field(name: &str, ty: FieldType) -> FieldDef {
    FieldDef {
        name: self::name(name),
        ty,
        nullable: true,
    }
}

/// The catalog every test runs against: an `item` with a child table and its origin fields, a
/// `gl_entry` (a ledger entry) and a `stock_entry`, both with origin fields.
pub fn catalog() -> Catalog {
    let line = EntityDef {
        name: name("item_line"),
        kind: EntityKind::Child {
            parent: name("item"),
        },
        fields: vec![field("qty", decimal()), field("note", text())],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let item = EntityDef {
        name: name("item"),
        kind: EntityKind::Document,
        fields: vec![
            field("title", text()),
            field("qty", decimal()),
            field("origin_node", text()),
            field("origin_id", text()),
            field("origin_version", FieldType::Int),
        ],
        children: vec![ChildTableDef {
            field: name("lines"),
            entity: name("item_line"),
        }],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let gl = EntityDef {
        name: name("gl_entry"),
        kind: EntityKind::Document,
        fields: vec![
            field("account", text()),
            field("amount", decimal()),
            field("origin_node", text()),
            field("origin_id", text()),
            field("origin_version", FieldType::Int),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    let stock = EntityDef {
        name: name("stock_entry"),
        kind: EntityKind::Document,
        fields: vec![
            field("warehouse", text()),
            field("qty", decimal()),
            field("origin_node", text()),
            field("origin_id", text()),
            field("origin_version", FieldType::Int),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    Catalog::new(
        1,
        Collation::new("en").unwrap_or_else(|e| panic!("{e}")),
        vec![item, line, gl, stock],
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

/// A fresh backend holding the test catalog.
pub fn backend() -> FakeBackend {
    FakeBackend::new()
}

/// The sender every test replicates from.
pub fn source() -> SourceId {
    SourceId::new("head.office.app#hub").unwrap_or_else(|e| panic!("{e}"))
}

/// A field map from text and int parts.
pub fn fields(parts: &[(&str, Value)]) -> Fields {
    parts.iter().map(|(k, v)| (name(k), v.clone())).collect()
}

/// A decimal value from text (exact, never a float).
pub fn dec(text: &str) -> Value {
    Value::Decimal(rust_decimal::Decimal::from_str(text).unwrap_or_else(|e| panic!("{e}")))
}

/// An item document at `version` with one child row.
pub fn item_document(id: RecordId, title: &str, qty: &str, version: i64) -> DocumentData {
    let mut children = BTreeMap::new();
    children.insert(
        name("lines"),
        vec![ChildRow {
            id: RecordId::new_v7(),
            fields: fields(&[
                ("qty", dec("2.00")),
                ("note", Value::Text("row".to_owned())),
            ]),
        }],
    );
    DocumentData {
        entity: name("item"),
        id,
        fields: fields(&[
            ("title", Value::Text(title.to_owned())),
            ("qty", dec(qty)),
            ("origin_node", Value::Null),
            ("origin_id", Value::Null),
            ("origin_version", Value::Int(version)),
        ]),
        children,
    }
}

/// A ledger entry document.
pub fn gl_document(id: RecordId, account: &str, amount: &str) -> DocumentData {
    DocumentData {
        entity: name("gl_entry"),
        id,
        fields: fields(&[
            ("account", Value::Text(account.to_owned())),
            ("amount", dec(amount)),
            ("origin_node", Value::Null),
            ("origin_id", Value::Null),
            ("origin_version", Value::Int(0)),
        ]),
        children: BTreeMap::new(),
    }
}

/// A received record for `document` at `version`, with `entries`.
pub fn record(
    document: DocumentData,
    version: u64,
    seq: u64,
    entries: Vec<DocumentData>,
) -> ReceivedRecord {
    ReceivedRecord {
        name: RecordName::new(&format!("{}:{}", document.entity.as_str(), document.id))
            .unwrap_or_else(|e| panic!("{e}")),
        version,
        seq,
        source: source(),
        topic: None,
        conversation: None,
        meta_version: 1,
        document,
        entries,
    }
}

/// Opens a read-write transaction on `backend`.
pub async fn begin(backend: &FakeBackend) -> spot_storage_fake::FakeTx {
    backend
        .begin(
            Arc::new(catalog()),
            TxOptions::new(name("replica_apply"), Isolation::ReadCommitted),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"))
}

/// Loads a document read-only, returning `None` when it is absent.
pub async fn load(backend: &FakeBackend, entity: &str, id: RecordId) -> Option<Document> {
    let mut tx = backend
        .begin(
            Arc::new(catalog()),
            TxOptions::new(name("replica_apply_load"), Isolation::ReadCommitted).read_only(),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let stored = tx.load(&name(entity), id, RowLock::None).await;
    let _ = tx.rollback().await;
    stored.unwrap_or_else(|e| panic!("{e}"))
}

/// Applies `record` with `ctx` in a fresh transaction.
pub async fn apply_once(
    backend: &FakeBackend,
    record: &ReceivedRecord,
    ctx: &ApplyCtx<'_>,
) -> ApplyOutcome {
    let tx = begin(backend).await;
    spot_replica_apply::apply(tx, record, ctx).await
}

/// An [`OwnerCheck`] that accepts `owner` and treats everything else as a violation.
pub struct Owners {
    /// The node that owns every record.
    pub owner: String,
    /// Whether the receiver owns the record (the rebuild case) or holds it as a mirror.
    pub owned: bool,
}

impl Owners {
    /// The hub owns everything; the receiver holds mirrors.
    pub fn mirror() -> Self {
        Self {
            owner: source().as_str().to_owned(),
            owned: false,
        }
    }
}

impl OwnerCheck for Owners {
    fn allow_replica(
        &self,
        sender: &SourceId,
        _record: &ReceivedRecord,
        _at: i64,
    ) -> Result<Ownership, Reason> {
        if sender.as_str() == self.owner {
            Ok(if self.owned {
                Ownership::Owned
            } else {
                Ownership::Mirror
            })
        } else {
            Err(Reason::NotOwner {
                owner: self.owner.clone(),
            })
        }
    }
}
