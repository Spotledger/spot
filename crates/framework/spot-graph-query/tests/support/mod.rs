//! Shared fixtures: a four-DocType chain (purchase order, receipt, invoice, payment) in the
//! storage fake, the matching graph model and permission snapshot, and an in-memory executor
//! that answers the same questions from values alone.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_graph_model::GraphModel;
use spot_graph_query::{
    GraphExecutor, GraphQuery, GraphQueryError, GraphResult, Invocation, NodeData, NodeSource,
    Step, walk,
};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::model::{ModelTexts, Principal};
use spot_permissions::{PermissionEngine, Snapshot, load};
use spot_storage_contract::schema::{
    EntityDef, EntityKind, FieldDef, FieldType as StorageType, LinkCheck, LinkTarget,
};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, EdgeSpec, Fields, Isolation, Name, NodeRef, RecordId,
    StorageBackend, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;

/// The chain's DocTypes, and the link field each one holds: `superseded_by` (purchase order to
/// purchase order), `po` (receipt to purchase order), `grn` (invoice to receipt), `invoice`
/// (payment to invoice).
pub const DOCTYPES: [&str; 4] = ["po", "grn", "invoice", "payment"];

/// The DocType grants. `Auditor` reads everything. `Reader` reads purchase orders and invoices,
/// not receipts or payments. `Owner` reads the payments its user created.
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
role = "Reader"
doctype = "po"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "invoice"
rights = ["read"]
[[entries]]
role = "Owner"
doctype = "payment"
rights = ["read"]
if_owner = true
"#;

pub fn n(text: &str) -> Name {
    Name::new(text).expect("a valid name")
}

/// The declared fields of each DocType, by name, with their storage types.
fn declared(entity: &str) -> Vec<(&'static str, StorageType)> {
    let link = |to: &str| StorageType::Link {
        target: LinkTarget::Entity(n(to)),
        check: LinkCheck::Database,
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
        _ => Vec::new(),
    }
}

/// The storage catalog of the chain.
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

/// The effective metadata of the chain: the graph model and the permission snapshot derive from it.
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
    ])
    .expect("definitions")
    .effective()
}

pub fn graph_model() -> Arc<GraphModel> {
    Arc::new(GraphModel::derive(&meta()))
}

/// The permission engine over the chain's grants.
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

pub fn invocation<'a>(principal: &'a Principal, engine: &'a PermissionEngine) -> Invocation<'a> {
    Invocation { principal, engine }
}

/// Runs `query` through `executor` for `principal`.
pub async fn run<E: GraphExecutor>(
    executor: &mut E,
    query: &GraphQuery,
    principal: &Principal,
) -> Result<GraphResult, GraphQueryError> {
    let engine = engine();
    executor
        .execute(query, &invocation(principal, &engine))
        .await
}

/// The nodes a result returns, as (entity, id) pairs with their depths.
pub fn depths(result: &GraphResult) -> BTreeMap<NodeRef, u16> {
    result
        .nodes
        .iter()
        .map(|n| (n.node.clone(), n.depth))
        .collect()
}

/// The neighbours of `node` along `step`, over explicit values: the same rule storage applies,
/// computed from the records' link values.
pub fn neighbours(
    nodes: &BTreeMap<NodeRef, NodeData>,
    node: &NodeRef,
    step: &Step,
) -> Vec<NodeRef> {
    let mut out: BTreeSet<NodeRef> = BTreeSet::new();
    let spec: &EdgeSpec = &step.spec;
    let outgoing = matches!(
        step.direction,
        spot_storage_contract::Direction::Outgoing | spot_storage_contract::Direction::Both
    );
    let incoming = matches!(
        step.direction,
        spot_storage_contract::Direction::Incoming | spot_storage_contract::Direction::Both
    );
    if outgoing
        && node.entity == spec.entity
        && let Some(Value::Id(target)) = nodes.get(node).and_then(|d| d.values.get(&spec.field))
        && let Some(found) = nodes.keys().find(|k| k.id == *target)
    {
        out.insert(found.clone());
    }
    if incoming {
        for (holder, data) in nodes {
            if holder.entity == spec.entity
                && data.values.get(&spec.field) == Some(&Value::Id(node.id))
            {
                out.insert(holder.clone());
            }
        }
    }
    out.remove(node);
    out.into_iter().collect()
}

/// A [`NodeSource`] over explicit records.
pub struct MemSource {
    pub nodes: BTreeMap<NodeRef, NodeData>,
}

impl NodeSource for MemSource {
    async fn load(&mut self, node: &NodeRef) -> Result<Option<NodeData>, GraphQueryError> {
        Ok(self.nodes.get(node).cloned())
    }

    async fn step(
        &mut self,
        node: &NodeRef,
        step: &Step,
        _max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        Ok(neighbours(&self.nodes, node, step))
    }
}

/// An executor that answers from explicit records, with the same compile and walk as storage.
pub struct MemExecutor {
    pub model: Arc<GraphModel>,
    pub nodes: BTreeMap<NodeRef, NodeData>,
}

impl GraphExecutor for MemExecutor {
    async fn execute(
        &mut self,
        query: &GraphQuery,
        who: &Invocation<'_>,
    ) -> Result<GraphResult, GraphQueryError> {
        let steps = spot_graph_query::compile(&self.model, query)?;
        let mut source = MemSource {
            nodes: self.nodes.clone(),
        };
        walk(&mut source, &steps, query, who).await
    }
}

/// The node data of the chain's records, as the storage executor reads them (`owner` is the
/// creator field).
pub fn node_data(docs: &[DocumentData]) -> BTreeMap<NodeRef, NodeData> {
    docs.iter()
        .map(|d| {
            (
                NodeRef {
                    entity: d.entity.clone(),
                    id: d.id,
                },
                NodeData {
                    values: d.fields.clone(),
                    state: None,
                    created_by: match d.fields.get(&n("owner")) {
                        Some(Value::Text(t)) => Some(t.clone()),
                        _ => None,
                    },
                },
            )
        })
        .collect()
}

/// Field grants on `payment.kind`, a field in the `bank` group: `Auditor` reads it, `Masked`
/// sees the masked form, `Plain` has no grant and so cannot see it.
pub const FIELD_GRANTS: &str = r#"
[[entries]]
role = "Auditor"
doctype = "payment"
target = { group = "bank" }
access = ["read"]
[[entries]]
role = "Masked"
doctype = "payment"
target = { group = "bank" }
access = ["read_masked"]
"#;

/// Extra DocType grants for the protected fixture: `Plain` and `Masked` read purchase orders,
/// receipts' neighbours (invoices) and payments; `Plain` has no field grant.
pub const PROTECTED_GRANTS: &str = r#"
[[entries]]
role = "Plain"
doctype = "invoice"
rights = ["read"]
[[entries]]
role = "Plain"
doctype = "payment"
rights = ["read"]
[[entries]]
role = "Masked"
doctype = "invoice"
rights = ["read"]
[[entries]]
role = "Masked"
doctype = "payment"
rights = ["read"]
"#;

/// The permission engine with `payment.kind` protected (group `bank`, masked to its last
/// character), plus the given shares: each `(record, user)` gives read on that record only.
pub fn protected_engine(shares: &[(RecordId, &str)]) -> Arc<PermissionEngine> {
    use spot_permissions::model::{FieldFacts, GroupName, MaskFormat};
    let share_text: String = shares
        .iter()
        .map(|(doc, user)| {
            format!("[[entries]]\ndoc = \"{doc}\"\nuser = \"{user}\"\nrights = [\"read\"]\n")
        })
        .collect();
    let meta = meta();
    let model = load(&ModelTexts {
        grants: format!("{GRANTS}{PROTECTED_GRANTS}"),
        field_grants: FIELD_GRANTS.to_owned(),
        groups: "[[entries]]\nname = \"bank\"\n".to_owned(),
        shares: share_text,
    })
    .expect("the protected model loads");
    let snapshot = Snapshot::build(&meta, &model)
        .expect("snapshot")
        .with_field_facts(
            "payment",
            vec![FieldFacts {
                path: "kind".to_owned(),
                groups: vec![GroupName("bank".to_owned())],
                tags: Vec::new(),
                mask: Some(MaskFormat::LastChars(1)),
            }],
        )
        .expect("field facts");
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}
