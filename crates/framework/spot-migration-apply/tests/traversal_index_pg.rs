//! The reverse index of a traversable Link field, planned and applied on a real PostgreSQL
//! (WP-STO-07.T3, F2). Skipped with a SKIPPED line when `SPOT_TEST_PG_URL` is unset and failing
//! when `SPOT_REQUIRE_PG=1` is.
//!
//! A 100,000-edge fixture (a tree of fan-out 8, every row links its parent) is loaded without
//! the index. The plan for making `part.owner` traversable carries the index; the backend
//! planner builds it online from the catalogue that declares it; `EXPLAIN` of a depth-3
//! incoming traversal then uses the index and not a scan.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_meta::{Definition, FieldSpec, FieldType};
use spot_migration_plan::{
    ChangeKind, CostModel, DocTypeDescription, RiskTable, SchemaPlan, SystemDescription,
    TraversalLink, TraversalState, plan_with_traversal,
};
use spot_pg_migrate::{ApplyOptions, LiveSchema, apply, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, Collation, EntityDef, EntityKind, FieldDef, IndexDef, IndexMethod, LinkCheck,
    LinkTarget, Name,
};

const EDGES: u32 = 100_000;

fn n(text: &str) -> Name {
    Name::new(text).unwrap()
}

/// `part(owner -> part)`, with the given indexes.
fn catalog(indexes: Vec<IndexDef>) -> Catalog {
    let part = EntityDef {
        name: n("part"),
        kind: EntityKind::Document,
        fields: vec![FieldDef {
            name: n("owner"),
            ty: spot_storage_contract::FieldType::Link {
                target: LinkTarget::Entity(n("part")),
                check: LinkCheck::Database,
            },
            nullable: true,
        }],
        children: vec![],
        indexes,
        exclusions: vec![],
        search: None,
    };
    Catalog::new(1, Collation::new("en").unwrap(), vec![part]).unwrap()
}

/// The system the plan reads: one DocType with a Link field.
fn system() -> SystemDescription {
    let mut definition = Definition::new("part");
    definition.fields = vec![FieldSpec::new("owner", FieldType::Link)];
    SystemDescription {
        doctypes: vec![DocTypeDescription {
            definition,
            permissions: Vec::new(),
            views: Default::default(),
        }],
        ..SystemDescription::new()
    }
}

/// The plan's index as the catalogue declares it.
fn index_def(index: &spot_migration_plan::TraversalIndex) -> IndexDef {
    IndexDef {
        name: n(&index.name),
        fields: vec![n(&index.column)],
        unique: false,
        method: IndexMethod::BTree,
        opclass: None,
        include: vec![],
    }
}

/// The `EXPLAIN` text of a depth-3 incoming traversal from the root.
async fn explain(client: &tokio_postgres::Client) -> String {
    let rows = client
        .query(
            "EXPLAIN WITH RECURSIVE reach(id, depth) AS (\
               SELECT id, 0 FROM part WHERE owner IS NULL \
               UNION ALL \
               SELECT c.id, r.depth + 1 FROM part c JOIN reach r ON c.owner = r.id \
               WHERE r.depth < 3) \
             SELECT id FROM reach",
            &[],
        )
        .await
        .unwrap();
    rows.iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_planned_reverse_index_serves_a_depth_3_traversal() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let client = db.raw_client().await;

    // The table, loaded with the edges and no traversal index.
    let bare = catalog(vec![]);
    let live = LiveSchema::read(&backend).await.unwrap();
    apply(&backend, &plan(&live, &bare), ApplyOptions::default())
        .await
        .unwrap();
    client
        .batch_execute(&format!(
            "WITH g AS (SELECT n, gen_random_uuid() AS id FROM generate_series(1, {EDGES}) n) \
             INSERT INTO part (id, version, owner) \
             SELECT g.id, 1, p.id FROM g LEFT JOIN g p ON p.n = (g.n + 6) / 8 \
             AND g.n > 1; \
             ANALYZE part"
        ))
        .await
        .unwrap();
    let count: i64 = client
        .query_one("SELECT count(*) FROM part", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, i64::from(EDGES));

    // Without the index the traversal scans.
    let before = explain(&client).await;
    assert!(
        before.contains("Seq Scan on part"),
        "no index yet, so a scan is expected:\n{before}"
    );

    // The release plan for making `part.owner` traversable carries the index.
    let from = TraversalState::default();
    let to = TraversalState {
        traversable: vec![TraversalLink::new("part", "owner")],
        ..TraversalState::default()
    };
    let release = plan_with_traversal(
        &system(),
        &system(),
        &SchemaPlan::default(),
        &RiskTable::builtin(),
        &CostModel::default(),
        &from,
        &to,
    )
    .unwrap();
    let adds: Vec<_> = release
        .changes
        .iter()
        .filter(|c| c.kind == ChangeKind::SchemaAddIndex)
        .collect();
    assert_eq!(adds.len(), 1, "{:?}", release.changes);
    let wanted = spot_migration_plan::traversal::wanted(&system(), &to).unwrap();
    assert_eq!(adds[0].target, format!("part.{}", wanted[0].name));

    // The backend planner builds exactly that index online from the catalogue.
    let with_index = catalog(vec![index_def(&wanted[0])]);
    let live = LiveSchema::read(&backend).await.unwrap();
    let migration = plan(&live, &with_index);
    let keys: Vec<_> = migration.steps.iter().map(|s| s.key.clone()).collect();
    // The index build, then the planner's own catalogue-version step.
    assert_eq!(
        keys,
        [
            format!("create_index:part.{}", wanted[0].name),
            "finalise".to_owned()
        ]
    );
    apply(&backend, &migration, ApplyOptions::default())
        .await
        .unwrap();
    client.batch_execute("ANALYZE part").await.unwrap();

    // Re-planning against the live schema is empty: idempotent.
    let live = LiveSchema::read(&backend).await.unwrap();
    assert!(
        plan(&live, &with_index)
            .steps
            .iter()
            .all(|s| !s.key.starts_with("create_index")),
        "the index is not built twice"
    );

    // The depth-3 traversal now uses the reverse index and no scan of the table.
    let after = explain(&client).await;
    assert!(
        after.contains(&wanted[0].name),
        "the traversal must use the reverse index:\n{after}"
    );
    assert!(
        !after.contains("Seq Scan on part"),
        "no table scan once the index exists:\n{after}"
    );
}
