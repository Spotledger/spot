//! `spot migrate` creates the platform's non-document tables through the plan, and a second run
//! plans nothing (WP-PRD-01.T11).
//!
//! Real PostgreSQL: the extra-table hook is the migrator's, and a fake database would prove
//! nothing about the DDL the owning crates publish. `SPOT_TEST_PG_URL` selects the server;
//! `SPOT_REQUIRE_PG=1` makes a missing one loud.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_cli::schema;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation, EntityDef, EntityKind, FieldDef, FieldType, Name};

/// The tables edge sync and the mail sent log write to: what a site cannot write without.
const REQUIRED: [&str; 7] = [
    "spot_graph_edge",
    "spot_edge_drift_report",
    "spot_edge_drift_entry",
    "spot_edge_repair_audit",
    "spot_mail_sent_log",
    "spot_audit_chain",
    "spot_audit_checkpoint",
];

fn empty_catalog() -> Catalog {
    Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap()
}

/// One document table, so the plan carries document steps beside the extra ones.
fn party_catalog() -> Catalog {
    let entity = EntityDef {
        name: Name::new("party").unwrap(),
        kind: EntityKind::Document,
        fields: vec![FieldDef {
            name: Name::new("title").unwrap(),
            ty: FieldType::Text { max_chars: None },
            nullable: false,
        }],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    Catalog::new(1, Collation::new("und").unwrap(), vec![entity]).unwrap()
}

/// The extra-table steps of a plan, in order.
fn extra_steps(plan: &spot_pg_migrate::MigrationPlan) -> Vec<&str> {
    plan.steps
        .iter()
        .map(|step| step.key.as_str())
        .filter(|key| key.starts_with("extra_table:"))
        .collect()
}

#[tokio::test]
async fn a_fresh_database_gets_the_platform_tables_and_a_second_run_plans_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = empty_catalog();

    let plan = schema::plan_with_platform_tables(&backend, &catalog)
        .await
        .unwrap();
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    assert_eq!(
        extra_steps(&plan),
        vec![
            "extra_table:spot_graph_edge",
            "extra_table:spot_graph_edge_owner_idx",
            "extra_table:spot_edge_drift_report",
            "extra_table:spot_edge_drift_entry",
            "extra_table:spot_edge_repair_audit",
            "extra_table:spot_mail_sent_log",
            "extra_table:spot_mail_sent_log_recorded_idx",
            "extra_table:spot_audit_chain",
            "extra_table:spot_audit_checkpoint",
            "extra_table:spot_condition_open",
            "extra_table:spot_condition_history",
            "extra_table:spot_condition_history_subject_idx",
            "extra_table:spot_node_role_state",
            "extra_table:spot_capture_watermark",
        ],
        "the owning crates' DDL, one statement per entry, a table before its index"
    );
    // Before the apply: what a serve start-up refuses over.
    let pending = schema::pending_platform_tables(&backend, &catalog)
        .await
        .unwrap();
    assert_eq!(pending.len(), 14);

    schema::refusals(&plan).unwrap();
    schema::apply(&backend, &plan, None).await.unwrap();
    let live = spot_pg_migrate::LiveSchema::read(&backend).await.unwrap();
    for name in REQUIRED {
        assert!(
            live.tables.contains_key(name),
            "`{name}` was not created; the database holds {:?}",
            live.tables.keys().collect::<Vec<_>>()
        );
    }

    // The index is a step of its own, and it is a relation of the database even though the live
    // schema reports tables.
    let client = db.raw_client().await;
    let index = client
        .query_opt(
            concat!(
                "SELECT 1 ",
                "FROM pg_indexes WHERE schemaname = current_schema() AND indexname = $1"
            ),
            &[&"spot_graph_edge_owner_idx"],
        )
        .await
        .unwrap();
    assert!(index.is_some(), "the edge table's index was not created");

    // The regression this test exists for: a second run plans nothing at all, so a site is never
    // refused an online schema change because of a table the platform created itself.
    let again = schema::plan_with_platform_tables(&backend, &catalog)
        .await
        .unwrap();
    assert!(
        again.is_empty(),
        "a second run planned {:?}",
        again
            .steps
            .iter()
            .map(|s| s.key.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        schema::pending_platform_tables(&backend, &catalog)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_populated_database_gets_its_document_table_and_the_platform_tables_together() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = party_catalog();
    let plan = schema::plan_with_platform_tables(&backend, &catalog)
        .await
        .unwrap();
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    let keys: Vec<&str> = plan.steps.iter().map(|s| s.key.as_str()).collect();
    assert!(
        keys.iter().any(|key| key.starts_with("create_table:party")),
        "the document table is planned: {keys:?}"
    );
    assert_eq!(extra_steps(&plan).len(), 14);
    schema::apply(&backend, &plan, None).await.unwrap();
    let live = spot_pg_migrate::LiveSchema::read(&backend).await.unwrap();
    for name in REQUIRED.iter().chain(["party"].iter()) {
        assert!(live.tables.contains_key(*name), "`{name}` is missing");
    }
    let again = schema::plan_with_platform_tables(&backend, &catalog)
        .await
        .unwrap();
    assert!(
        again.is_empty(),
        "a second run planned {:?}",
        again
            .steps
            .iter()
            .map(|s| s.key.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn spot_migrate_on_a_new_site_reports_nothing_to_do_and_the_tables_are_there() {
    let Some(env) = support::Env::new().await else {
        return;
    };
    let _secret = env.bare_site();
    // A new site is provisioned through the same plan, so a migration straight after it has
    // nothing to do — and the tables the participants write to already exist.
    let out = env.ok(&["migrate", "--apply"]);
    assert!(out.contains("schema is up to date"), "{out}");
    let live = env.live().await;
    for name in REQUIRED {
        assert!(
            live.tables.contains_key(name),
            "`{name}` is missing after new-site"
        );
    }
}
