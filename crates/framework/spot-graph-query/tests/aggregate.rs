//! Repeated lines of one pair (WP-GRA-03.T9): an edge type that declares `aggregate = sum` merges
//! the lines the caller may read into one edge; without the declaration the pair stays ambiguous.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_graph_model::{AggregateHow, AggregateSpec, GraphModel, RelationSpec};
use spot_graph_query::{
    Direction, EdgeFieldAccess, EdgeProps, EdgeSelector, FactFields, GraphQuery, GraphQueryError,
    GraphResult, Include, StorageExecutor, merge_lines,
};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::model::ModelTexts;
use spot_permissions::{PermissionEngine, Snapshot, load};
use spot_storage_contract::schema::{
    EntityDef, EntityKind, FieldDef, FieldType as StorageType, LinkCheck, LinkTarget,
};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, DocumentStore, Fields, Isolation, Name, NodeRef, RecordId,
    StorageBackend, Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;
use support::{block_on, n, who};

const EDGE: &str = "bom";

const GRANTS: &str = r#"
[[entries]]
role = "Planner"
doctype = "part"
rights = ["read"]
[[entries]]
role = "Planner"
doctype = "bom_line"
rights = ["read"]
[[entries]]
role = "Clerk"
doctype = "part"
rights = ["read"]
[[entries]]
role = "Clerk"
doctype = "bom_line"
rights = ["read"]
if_owner = true
"#;

fn link(name: &str, to: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Link);
    field.options = Some(to.to_owned());
    field
}

fn definitions() -> Vec<Definition> {
    let mut part = Definition::new("part");
    part.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let mut line = Definition::new("bom_line");
    line.fields = vec![
        link("assembly", "part"),
        link("component", "part"),
        FieldSpec::new("quantity", FieldType::Int),
        FieldSpec::new("scrap", FieldType::Text),
        FieldSpec::new("created_by", FieldType::Text),
    ];
    vec![part, line]
}

fn model(aggregate: Option<AggregateSpec>) -> Arc<GraphModel> {
    let meta = Registry::new(definitions())
        .expect("definitions")
        .effective();
    let spec = RelationSpec {
        doctype: "bom_line".into(),
        from_field: "assembly".into(),
        to_field: "component".into(),
        edge_name: EDGE.into(),
        aggregate,
    };
    Arc::new(GraphModel::derive_relations(&meta, &[spec]))
}

fn sum_of_quantity() -> Option<AggregateSpec> {
    Some(AggregateSpec {
        property: "quantity".into(),
        how: AggregateHow::Sum,
    })
}

fn engine() -> Arc<PermissionEngine> {
    let meta = Registry::new(definitions())
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
                        ("scrap", text.clone()),
                        ("created_by", text),
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

fn line(a: RecordId, c: RecordId, quantity: i64, scrap: &str, by: &str) -> DocumentData {
    doc(
        "bom_line",
        RecordId::new_v7(),
        &[
            ("assembly", Value::Id(a)),
            ("component", Value::Id(c)),
            ("quantity", Value::Int(quantity)),
            ("scrap", scrap.into()),
            ("created_by", by.into()),
        ],
    )
}

/// Refuses the listed properties of every edge type.
struct Hide(Vec<&'static str>);

impl EdgeFieldAccess for Hide {
    fn edge_field_readable(&self, _edge_type: &str, property: &Name) -> bool {
        !self.0.contains(&property.as_str())
    }
}

/// The creator is not an edge property a caller compares.
fn hide_creator() -> Arc<dyn EdgeFieldAccess> {
    Arc::new(Hide(vec!["created_by"]))
}

struct Fixture {
    a: RecordId,
    b: RecordId,
}

/// Runs `query` from `a` over `lines`, as a user with `role`, under `model`.
fn run(
    model: Arc<GraphModel>,
    lines: Vec<DocumentData>,
    role: &str,
    user: &str,
    max_nodes: Option<u32>,
) -> (Fixture, Result<GraphResult, GraphQueryError>) {
    let fixture = Fixture {
        a: RecordId::new_v7(),
        b: RecordId::new_v7(),
    };
    let result = block_on(async {
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
        tx.insert(&doc("part", fixture.a, &[("name", "A".into())]))
            .await
            .expect("insert");
        tx.insert(&doc("part", fixture.b, &[("name", "B".into())]))
            .await
            .expect("insert");
        for l in lines {
            let mut l = l;
            // The helper builds lines on placeholders: bind them to this fixture's parts.
            l.fields.insert(n("assembly"), Value::Id(fixture.a));
            l.fields.insert(n("component"), Value::Id(fixture.b));
            tx.insert(&l).await.expect("insert");
        }
        tx.commit().await.expect("commit");
        let tx = backend
            .begin(
                cat.clone(),
                TxOptions::new(n("fixture"), Isolation::ReadCommitted),
            )
            .await
            .expect("begin");
        let facts = FactFields {
            state: None,
            created_by: Some(n("created_by")),
        };
        let mut executor = StorageExecutor::new(tx, model, facts, hide_creator());
        let mut builder = GraphQuery::builder()
            .from(vec![NodeRef {
                entity: n("part"),
                id: fixture.a,
            }])
            .follow(vec![EdgeSelector::new(EDGE)])
            .direction(Direction::Out)
            .max_depth(2)
            .include(Include {
                fields: vec![],
                edge_properties: true,
            });
        if let Some(max) = max_nodes {
            builder = builder.max_nodes(max);
        }
        let query = builder.build().unwrap();
        let principal = who(user, &[role]);
        let engine = engine();
        spot_graph_query::GraphExecutor::execute(
            &mut executor,
            &query,
            &spot_graph_query::Invocation {
                principal: &principal,
                engine: &engine,
            },
        )
        .await
    });
    (fixture, result)
}

fn placeholder() -> RecordId {
    RecordId::new_v7()
}

fn quantity(result: &GraphResult, b: RecordId) -> Option<Value> {
    result
        .nodes
        .iter()
        .find(|x| x.node.id == b)
        .and_then(|found| found.path.last())
        .and_then(|hop| hop.properties.get(&n("quantity")).cloned())
}

#[test]
fn two_lines_of_one_pair_sum() {
    let (f, result) = run(
        model(sum_of_quantity()),
        vec![
            line(placeholder(), placeholder(), 5, "none", "alice"),
            line(placeholder(), placeholder(), 3, "none", "alice"),
        ],
        "Planner",
        "alice",
        None,
    );
    let result = result.unwrap();
    assert_eq!(quantity(&result, f.b), Some(Value::Int(8)));
    assert_eq!(result.nodes.iter().filter(|x| x.node.id == f.b).count(), 1);
}

#[test]
fn a_line_the_caller_cannot_read_is_not_summed_in() {
    let lines = vec![
        line(placeholder(), placeholder(), 5, "none", "alice"),
        line(placeholder(), placeholder(), 3, "none", "alice"),
        line(placeholder(), placeholder(), 100, "none", "bob"),
    ];
    let (f, result) = run(
        model(sum_of_quantity()),
        lines.clone(),
        "Clerk",
        "alice",
        None,
    );
    assert_eq!(quantity(&result.unwrap(), f.b), Some(Value::Int(8)));
    let (f, result) = run(model(sum_of_quantity()), lines, "Planner", "alice", None);
    assert_eq!(quantity(&result.unwrap(), f.b), Some(Value::Int(108)));
}

#[test]
fn a_differing_property_between_lines_still_refuses_and_is_named() {
    let (_, result) = run(
        model(sum_of_quantity()),
        vec![
            line(placeholder(), placeholder(), 5, "none", "alice"),
            line(placeholder(), placeholder(), 3, "ten", "alice"),
        ],
        "Planner",
        "alice",
        None,
    );
    match result.unwrap_err() {
        GraphQueryError::AmbiguousRelationEdge {
            property, count, ..
        } => {
            assert_eq!(property.as_deref(), Some("scrap"));
            assert_eq!(count, 2);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_pair_with_one_line_is_unchanged() {
    let (f, result) = run(
        model(sum_of_quantity()),
        vec![line(placeholder(), placeholder(), 7, "none", "alice")],
        "Planner",
        "alice",
        None,
    );
    assert_eq!(quantity(&result.unwrap(), f.b), Some(Value::Int(7)));
}

#[test]
fn an_overflowing_sum_refuses() {
    let (_, result) = run(
        model(sum_of_quantity()),
        vec![
            line(placeholder(), placeholder(), i64::MAX, "none", "alice"),
            line(placeholder(), placeholder(), 1, "none", "alice"),
        ],
        "Planner",
        "alice",
        None,
    );
    assert_eq!(result.unwrap_err().code(), "graphq.edge_value_overflow");
}

#[test]
fn a_model_without_the_declaration_refuses_as_before() {
    let (_, result) = run(
        model(None),
        vec![
            line(placeholder(), placeholder(), 5, "none", "alice"),
            line(placeholder(), placeholder(), 3, "none", "alice"),
        ],
        "Planner",
        "alice",
        None,
    );
    match result.unwrap_err() {
        err @ GraphQueryError::AmbiguousRelationEdge { .. } => {
            assert_eq!(err.code(), "graphq.ambiguous_relation_edge");
            assert!(matches!(
                err,
                GraphQueryError::AmbiguousRelationEdge { property: None, .. }
            ));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn max_nodes_bounds_lines_not_merged_edges() {
    let lines = vec![
        line(placeholder(), placeholder(), 1, "none", "alice"),
        line(placeholder(), placeholder(), 1, "none", "alice"),
        line(placeholder(), placeholder(), 1, "none", "alice"),
    ];
    let (_, result) = run(model(sum_of_quantity()), lines, "Planner", "alice", Some(2));
    assert!(result.is_err(), "three lines exceed a bound of two");
}

fn props(items: &[(&str, Value)]) -> EdgeProps {
    items.iter().map(|(k, v)| (n(k), v.clone())).collect()
}

#[test]
fn decimals_sum_exactly_and_nulls_add_nothing() {
    let q = n("quantity");
    let dec = |t: &str| Value::Decimal(t.parse().unwrap());
    let merged = merge_lines(
        EDGE,
        Some(&q),
        &[
            props(&[("quantity", dec("1.10"))]),
            props(&[("quantity", dec("2.205"))]),
            props(&[("quantity", Value::Null)]),
        ],
    )
    .unwrap();
    assert_eq!(merged.get(&q), Some(&dec("3.305")));
    let none = merge_lines(
        EDGE,
        Some(&q),
        &[
            props(&[("quantity", Value::Null)]),
            props(&[("quantity", Value::Null)]),
        ],
    )
    .unwrap();
    assert!(!none.contains_key(&q));
}

#[test]
fn a_decimal_sum_that_does_not_fit_overflows() {
    let q = n("quantity");
    let big = Value::Decimal("79228162514264337593543950335".parse().unwrap());
    let err = merge_lines(
        EDGE,
        Some(&q),
        &[
            props(&[("quantity", big.clone())]),
            props(&[("quantity", big)]),
        ],
    )
    .unwrap_err();
    assert_eq!(err.code(), "graphq.edge_value_overflow");
}
