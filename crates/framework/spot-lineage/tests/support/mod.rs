//! Shared fixtures for the lineage tests: an order-to-cash chain (purchase order, receipt,
//! invoice, payment) and a bill of materials (`item` items linking to their parent item) in the
//! storage fake, with the matching graph model, permission snapshot and chain configuration.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_graph_model::GraphModel;
use spot_graph_query::{FactFields, NodeData, StorageExecutor};
use spot_lineage::ChainConfig;
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::model::{ModelTexts, Principal};
use spot_permissions::{PermissionEngine, Snapshot, load};
use spot_storage_contract::schema::{
    EntityDef, EntityKind, FieldDef, FieldType as StorageType, LinkCheck, LinkTarget,
};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, Fields, Isolation, Name, NodeRef, RecordId, StorageBackend,
    TxOptions, Value,
};
use spot_storage_fake::FakeBackend;

/// The chain's DocTypes, then the item DocType of the bill of materials.
pub const DOCTYPES: [&str; 5] = ["po", "grn", "invoice", "payment", "item"];

/// The DocType grants. `Auditor` reads everything. `Reader` reads purchase orders, invoices and
/// items, not receipts or payments. `Owner` reads the payments its user created.
pub const GRANTS: &str = r#"
[[entries]]
role = "Auditor"
doctype = "po"
rights = ["read"]
[[entries]]
role = "Auditor"
doctype = "grn"
rights = ["read"]
[[entries]]
role = "Auditor"
doctype = "invoice"
rights = ["read"]
[[entries]]
role = "Auditor"
doctype = "payment"
rights = ["read"]
[[entries]]
role = "Auditor"
doctype = "item"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "po"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "invoice"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "item"
rights = ["read"]
[[entries]]
role = "Owner"
doctype = "payment"
rights = ["read"]
if_owner = true
"#;

/// The chain configuration the crate ships for its tests.
pub fn config() -> ChainConfig {
    ChainConfig::defaults().expect("the built-in chain configuration parses")
}

pub fn n(text: &str) -> Name {
    Name::new(text).expect("a valid name")
}

/// The declared fields of each DocType, by name, with their storage types.
fn declared(entity: &str) -> Vec<(&'static str, StorageType)> {
    let link = |to: &str| StorageType::Link {
        target: LinkTarget::Entity(n(to)),
        check: LinkCheck::Database,
    };
    let decimal = StorageType::Decimal {
        precision: 18,
        scale: 6,
    };
    match entity {
        "po" => vec![
            ("title", StorageType::Text { max_chars: None }),
            ("owner", StorageType::Text { max_chars: None }),
            ("superseded_by", link("po")),
        ],
        "grn" => vec![
            ("owner", StorageType::Text { max_chars: None }),
            ("po", link("po")),
        ],
        "invoice" => vec![
            ("owner", StorageType::Text { max_chars: None }),
            ("grn", link("grn")),
        ],
        "payment" => vec![
            ("owner", StorageType::Text { max_chars: None }),
            ("invoice", link("invoice")),
            ("kind", StorageType::Text { max_chars: None }),
        ],
        "item" => vec![
            ("title", StorageType::Text { max_chars: None }),
            ("parent_item", link("item")),
            ("qty", decimal),
        ],
        _ => Vec::new(),
    }
}

/// The storage catalog of the fixture.
pub fn catalog() -> Arc<Catalog> {
    let entities = DOCTYPES
        .iter()
        .map(|entity| EntityDef {
            name: n(entity),
            kind: EntityKind::Document,
            fields: declared(entity)
                .into_iter()
                .map(|(name, ty)| FieldDef {
                    name: n(name),
                    ty,
                    nullable: true,
                })
                .collect(),
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        })
        .collect();
    Arc::new(Catalog::new(1, Collation::new("en").expect("a locale"), entities).expect("a catalog"))
}

/// A stored document of `entity` with `values`; every other declared field is null.
pub fn record(entity: &str, id: RecordId, values: &[(&str, Value)]) -> DocumentData {
    let mut fields: Fields = declared(entity)
        .into_iter()
        .map(|(name, _)| (n(name), Value::Null))
        .collect();
    for (name, value) in values {
        fields.insert(n(name), value.clone());
    }
    DocumentData {
        entity: n(entity),
        id,
        fields,
        children: BTreeMap::new(),
    }
}

/// Stores `docs` in one committed transaction.
pub async fn store(docs: &[DocumentData]) -> (FakeBackend, Arc<Catalog>) {
    let backend = FakeBackend::new();
    let cat = catalog();
    backend.prepare(&cat).await.expect("prepare");
    let mut tx = backend
        .begin(
            cat.clone(),
            TxOptions::new(n("fixture"), Isolation::ReadCommitted),
        )
        .await
        .expect("begin");
    for doc in docs {
        spot_storage_contract::DocumentStore::insert(&mut tx, doc)
            .await
            .expect("insert");
    }
    spot_storage_contract::Transaction::commit(tx)
        .await
        .expect("commit");
    (backend, cat)
}

fn definition(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut definition = Definition::new(name);
    definition.fields = fields;
    definition
}

fn link(name: &str, to: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Link);
    field.options = Some(to.to_owned());
    field
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn decimal(name: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Decimal);
    field.scale = Some(6);
    field
}

/// The effective metadata of the fixture: the graph model and the permission snapshot derive
/// from it.
pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(vec![
        definition(
            "po",
            vec![text("title"), text("owner"), link("superseded_by", "po")],
        ),
        definition("grn", vec![text("owner"), link("po", "po")]),
        definition("invoice", vec![text("owner"), link("grn", "grn")]),
        definition(
            "payment",
            vec![text("owner"), link("invoice", "invoice"), text("kind")],
        ),
        definition(
            "item",
            vec![text("title"), link("parent_item", "item"), decimal("qty")],
        ),
    ])
    .expect("definitions")
    .effective()
}

pub fn graph_model() -> Arc<GraphModel> {
    Arc::new(GraphModel::derive(&meta()))
}

/// The permission engine over the fixture's grants.
pub fn engine() -> Arc<PermissionEngine> {
    let meta = meta();
    let model = load(&ModelTexts {
        grants: GRANTS.to_owned(),
        ..Default::default()
    })
    .expect("grants load");
    let snapshot = Snapshot::build(&meta, &model).expect("snapshot");
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// A principal with the given roles.
pub fn who(user: &str, roles: &[&str]) -> Principal {
    Principal::with_roles(user, roles)
}

/// Runs a future that completes without waiting on anything else.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut cx = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

/// Opens a storage executor over a fresh read-only transaction.
pub async fn executor(
    backend: &FakeBackend,
) -> StorageExecutor<impl spot_storage_contract::Transaction> {
    let tx = backend
        .begin(
            catalog(),
            TxOptions::new(n("lineage"), Isolation::RepeatableRead).read_only(),
        )
        .await
        .expect("begin");
    StorageExecutor::new(
        tx,
        graph_model(),
        FactFields {
            state: None,
            created_by: Some(n("owner")),
        },
        std::sync::Arc::new(spot_graph_query::UnrestrictedEdgeFields),
    )
}

/// The node data of the fixture's records, as the storage executor reads them (`owner` is the
/// creator field).
pub fn node_data(docs: &[DocumentData]) -> BTreeMap<NodeRef, NodeData> {
    docs.iter()
        .map(|doc| {
            (
                NodeRef {
                    entity: doc.entity.clone(),
                    id: doc.id,
                },
                NodeData {
                    values: doc.fields.clone(),
                    state: None,
                    created_by: match doc.fields.get(&n("owner")) {
                        Some(Value::Text(text)) => Some(text.clone()),
                        _ => None,
                    },
                },
            )
        })
        .collect()
}

pub fn at(entity: &str, id: RecordId) -> NodeRef {
    NodeRef {
        entity: n(entity),
        id,
    }
}
