//! Relation edges over storage (WP-GRA-03.T5): a bill of materials kept as records of a relation
//! DocType is followed by the storage executor, with the quantities of its lines as edge
//! properties. Every scenario runs on the storage fake, and on PostgreSQL when `SPOT_TEST_PG_URL`
//! is set.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_graph_model::{GraphModel, RelationSpec};
use spot_graph_query::{
    Direction, EdgeFieldAccess, EdgeSelector, FactFields, GraphQuery, GraphQueryError, GraphResult,
    Include, StorageExecutor,
};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::model::ModelTexts;
use spot_permissions::{PermissionEngine, Snapshot, load};
use spot_storage_contract::schema::{
    EntityDef, EntityKind, FieldDef, FieldType as StorageType, LinkCheck, LinkTarget,
};
use spot_storage_contract::{
    Catalog, CmpOp, Collation, DocumentData, DocumentStore, ErrorKind, Expr, FieldRef, Fields,
    Isolation, Name, NodeRef, RecordId, StorageBackend, Transaction, TxOptions, Value, Version,
};
use spot_storage_fake::FakeBackend;
use support::{block_on, n, who};

const EDGE: &str = "bom";

const GRANTS: &str = r#"
[[entries]]
role = "Engineer"
doctype = "part"
rights = ["read"]
[[entries]]
role = "Engineer"
doctype = "bom_line"
rights = ["read"]
[[entries]]
role = "Buyer"
doctype = "part"
rights = ["read"]
"#;

fn link(name: &str, to: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Link);
    field.options = Some(to.to_owned());
    field
}

fn model() -> Arc<GraphModel> {
    let mut part = Definition::new("part");
    part.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let mut line = Definition::new("bom_line");
    line.fields = vec![
        link("assembly", "part"),
        link("component", "part"),
        FieldSpec::new("quantity", FieldType::Int),
        FieldSpec::new("note", FieldType::Text),
    ];
    let meta = Registry::new(vec![part, line])
        .expect("definitions")
        .effective();
    let spec = RelationSpec {
        doctype: "bom_line".into(),
        from_field: "assembly".into(),
        to_field: "component".into(),
        edge_name: EDGE.into(),
        aggregate: None,
    };
    Arc::new(GraphModel::derive_relations(&meta, &[spec]))
}

fn engine() -> Arc<PermissionEngine> {
    let mut part = Definition::new("part");
    part.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let mut line = Definition::new("bom_line");
    line.fields = vec![
        link("assembly", "part"),
        link("component", "part"),
        FieldSpec::new("quantity", FieldType::Int),
        FieldSpec::new("note", FieldType::Text),
    ];
    let meta = Registry::new(vec![part, line])
        .expect("definitions")
        .effective();
    let rules = load(&ModelTexts {
        grants: GRANTS.to_owned(),
        ..Default::default()
    })
    .expect("grants");
    Arc::new(PermissionEngine::new(Arc::new(
        Snapshot::build(&meta, &rules).expect("snapshot"),
    )))
}

fn catalog() -> Arc<Catalog> {
    let link = |to: &str| StorageType::Link {
        target: LinkTarget::Entity(n(to)),
        check: LinkCheck::Database,
    };
    let text = StorageType::Text { max_chars: None };
    let def = |name: &str, fields: Vec<(&str, StorageType)>| EntityDef {
        name: n(name),
        kind: EntityKind::Document,
        fields: fields
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
    };
    Arc::new(
        Catalog::new(
            1,
            Collation::new("en").expect("a locale"),
            vec![
                def("part", vec![("name", text.clone())]),
                def(
                    "bom_line",
                    vec![
                        ("assembly", link("part")),
                        ("component", link("part")),
                        ("quantity", StorageType::Int),
                        ("note", text),
                    ],
                ),
            ],
        )
        .expect("a catalog"),
    )
}

fn doc(entity: &str, id: RecordId, values: &[(&str, Value)]) -> DocumentData {
    let fields: Fields = values.iter().map(|(k, v)| (n(k), v.clone())).collect();
    DocumentData {
        entity: n(entity),
        id,
        fields,
        children: BTreeMap::new(),
    }
}

fn part(id: RecordId, name: &str) -> DocumentData {
    doc("part", id, &[("name", name.into())])
}

fn line(id: RecordId, parent: RecordId, component: RecordId, quantity: i64) -> DocumentData {
    doc(
        "bom_line",
        id,
        &[
            ("assembly", Value::Id(parent)),
            ("component", Value::Id(component)),
            ("quantity", Value::Int(quantity)),
            ("note", Value::Null),
        ],
    )
}

fn node(entity: &str, id: RecordId) -> NodeRef {
    NodeRef {
        entity: n(entity),
        id,
    }
}

/// Refuses the listed properties of every edge type.
struct Hide(Vec<&'static str>);

impl EdgeFieldAccess for Hide {
    fn edge_field_readable(&self, _edge_type: &str, property: &Name) -> bool {
        !self.0.contains(&property.as_str())
    }
}

/// `a` uses two `b` and three `c`; `b` uses four `d`; `c` uses five `d`.
struct Bom {
    a: RecordId,
    b: RecordId,
    c: RecordId,
    d: RecordId,
    a_b: RecordId,
    a_c: RecordId,
    b_d: RecordId,
    c_d: RecordId,
}

impl Bom {
    fn new() -> Self {
        Self {
            a: RecordId::new_v7(),
            b: RecordId::new_v7(),
            c: RecordId::new_v7(),
            d: RecordId::new_v7(),
            a_b: RecordId::new_v7(),
            a_c: RecordId::new_v7(),
            b_d: RecordId::new_v7(),
            c_d: RecordId::new_v7(),
        }
    }

    fn docs(&self) -> Vec<DocumentData> {
        vec![
            part(self.a, "A"),
            part(self.b, "B"),
            part(self.c, "C"),
            part(self.d, "D"),
            line(self.a_b, self.a, self.b, 2),
            line(self.a_c, self.a, self.c, 3),
            line(self.b_d, self.b, self.d, 4),
            line(self.c_d, self.c, self.d, 5),
        ]
    }
}

async fn begin<B: StorageBackend>(backend: &B, cat: &Arc<Catalog>) -> B::Tx {
    backend
        .begin(
            cat.clone(),
            TxOptions::new(n("fixture"), Isolation::ReadCommitted),
        )
        .await
        .expect("begin")
}

async fn run<B: StorageBackend>(
    backend: &B,
    cat: &Arc<Catalog>,
    query: &GraphQuery,
    role: &str,
    access: Option<Arc<dyn EdgeFieldAccess>>,
) -> Result<GraphResult, GraphQueryError> {
    let tx = begin(backend, cat).await;
    let access = access.unwrap_or_else(|| Arc::new(Hide(vec![])));
    let mut executor = StorageExecutor::new(tx, model(), FactFields::default(), access);
    let principal = who("alice", &[role]);
    let engine = engine();
    let result = spot_graph_query::GraphExecutor::execute(
        &mut executor,
        query,
        &spot_graph_query::Invocation {
            principal: &principal,
            engine: &engine,
        },
    )
    .await;
    executor
        .into_transaction()
        .rollback()
        .await
        .expect("rollback");
    result
}

fn explode(start: RecordId, direction: Direction) -> spot_graph_query::GraphQueryBuilder {
    GraphQuery::builder()
        .from(vec![node("part", start)])
        .follow(vec![EdgeSelector::new(EDGE)])
        .direction(direction)
        .max_depth(5)
        .include(Include {
            fields: vec![],
            edge_properties: true,
        })
}

fn at(result: &GraphResult, id: RecordId) -> &spot_graph_query::GraphNode {
    result
        .nodes
        .iter()
        .find(|x| x.node.id == id)
        .unwrap_or_else(|| panic!("{id} is not in the result"))
}

fn quantity_of(found: &spot_graph_query::GraphNode) -> Option<&Value> {
    found
        .path
        .last()
        .and_then(|hop| hop.properties.get(&n("quantity")))
}

async fn scenario<B: StorageBackend>(backend: &B) {
    let cat = catalog();
    backend.prepare(&cat).await.expect("prepare");
    let bom = Bom::new();
    let mut tx = begin(backend, &cat).await;
    for d in bom.docs() {
        tx.insert(&d).await.expect("insert");
    }
    tx.commit().await.expect("commit");

    // The explosion: quantities as properties, the sum of the path products at the part used twice.
    let query = explode(bom.a, Direction::Out)
        .rollup_sum(n("quantity"))
        .build()
        .unwrap();
    let result = run(
        backend,
        &cat,
        &query,
        "Engineer",
        Some(Arc::new(Hide(vec![]))),
    )
    .await
    .unwrap();
    assert_eq!(result.nodes.len(), 4);
    assert_eq!(quantity_of(at(&result, bom.b)), Some(&Value::Int(2)));
    assert_eq!(quantity_of(at(&result, bom.c)), Some(&Value::Int(3)));
    let d = at(&result, bom.d);
    assert_eq!(d.rolled_up, Some(Value::Int(2 * 4 + 3 * 5)));
    assert_eq!(d.path_count, 2);
    assert_eq!(at(&result, bom.b).rolled_up, Some(Value::Int(2)));

    // Incoming: where is D used, with the quantity on each line.
    let query = explode(bom.d, Direction::In).build().unwrap();
    let result = run(backend, &cat, &query, "Engineer", None).await.unwrap();
    assert_eq!(quantity_of(at(&result, bom.b)), Some(&Value::Int(4)));
    assert_eq!(quantity_of(at(&result, bom.c)), Some(&Value::Int(5)));
    assert_eq!(at(&result, bom.a).depth, 2);

    // A caller who cannot read the relation DocType sees no edge, and no error.
    let query = explode(bom.a, Direction::Out).build().unwrap();
    let result = run(backend, &cat, &query, "Buyer", None).await.unwrap();
    assert_eq!(result.nodes.len(), 1);
    assert!(result.edges.is_empty());

    // A property the field rule hides is absent, and cannot be probed.
    let hide: Arc<dyn EdgeFieldAccess> = Arc::new(Hide(vec!["quantity"]));
    let result = run(backend, &cat, &query, "Engineer", Some(hide.clone()))
        .await
        .unwrap();
    assert_eq!(result.nodes.len(), 4);
    for found in result.nodes.iter().filter(|x| x.depth > 0) {
        assert!(
            !found
                .path
                .last()
                .unwrap()
                .properties
                .contains_key(&n("quantity"))
        );
    }
    let probe = explode(bom.a, Direction::Out)
        .edge_var(n("e"))
        .edge_filter(Expr::Cmp(
            Box::new(Expr::Field(FieldRef {
                alias: n("e"),
                field: n("quantity"),
            })),
            CmpOp::Ge,
            Box::new(Expr::Value(Value::Int(0))),
        ))
        .build()
        .unwrap();
    let refused = run(backend, &cat, &probe, "Engineer", Some(hide)).await;
    assert_eq!(refused.unwrap_err().code(), "graphq.unknown_edge_property");

    // A node with more relation records than the bound is a typed refusal, not a short answer.
    let tight = explode(bom.a, Direction::Out).max_nodes(2).build().unwrap();
    match run(backend, &cat, &tight, "Engineer", None).await {
        Err(GraphQueryError::Storage(error)) => {
            assert_eq!(error.kind, ErrorKind::TraversalTooLarge);
        }
        other => panic!("expected TraversalTooLarge, got {other:?}"),
    }

    // An update, a delete and a duplicate line are all reflected.
    let mut tx = begin(backend, &cat).await;
    tx.update(&line(bom.a_c, bom.a, bom.c, 7), Version::FIRST)
        .await
        .expect("update");
    tx.delete(&n("bom_line"), bom.b_d, Version::FIRST)
        .await
        .expect("delete");
    tx.commit().await.expect("commit");
    let query = explode(bom.a, Direction::Out)
        .rollup_sum(n("quantity"))
        .build()
        .unwrap();
    let result = run(
        backend,
        &cat,
        &query,
        "Engineer",
        Some(Arc::new(Hide(vec![]))),
    )
    .await
    .unwrap();
    assert_eq!(quantity_of(at(&result, bom.c)), Some(&Value::Int(7)));
    assert_eq!(at(&result, bom.d).rolled_up, Some(Value::Int(7 * 5)));
    assert_eq!(at(&result, bom.d).path_count, 1);

    let duplicate = RecordId::new_v7();
    let mut tx = begin(backend, &cat).await;
    tx.insert(&line(duplicate, bom.a, bom.b, 9))
        .await
        .expect("insert");
    tx.commit().await.expect("commit");
    let refused = run(backend, &cat, &query, "Engineer", None).await;
    assert_eq!(
        refused.unwrap_err().code(),
        "graphq.ambiguous_relation_edge"
    );
}

#[test]
fn a_bom_kept_as_records_explodes_with_its_quantities_on_the_fake() {
    block_on(scenario(&FakeBackend::new()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bom_kept_as_records_explodes_with_its_quantities_on_postgres() {
    use spot_postgres::PgBackend;
    use spot_postgres::testing::TestDb;
    let Some(db) = TestDb::create().await else {
        return;
    };
    let backend = PgBackend::connect(db.config()).await.expect("connect");
    scenario(&backend).await;
}
