//! Every table is `spot_`-prefixed, and databases that hold the old names are upgraded in place.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_edge_pg::schema::{DDL, prepare};
use spot_postgres::testing::TestDb;

const NEW: [&str; 4] = [
    "spot_graph_edge",
    "spot_edge_drift_report",
    "spot_edge_drift_entry",
    "spot_edge_repair_audit",
];
const OLD: [&str; 4] = [
    "graph_edge",
    "edge_drift_report",
    "edge_drift_entry",
    "edge_repair_audit",
];

/// The DDL as it was before the prefix.
fn old_ddl() -> String {
    DDL.replace("spot_", "")
}

async fn tables(client: &tokio_postgres::Client) -> Vec<String> {
    client
        .query(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = current_schema() ORDER BY table_name",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

async fn names(client: &tokio_postgres::Client, sql: &str) -> Vec<String> {
    client
        .query(sql, &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

const EDGE_ID: &str = "00000000-0000-0000-0000-000000000001";

async fn insert_old_rows(client: &tokio_postgres::Client) {
    client.batch_execute(&old_ddl()).await.unwrap();
    client
        .batch_execute(&format!(
            "INSERT INTO graph_edge VALUES ('t','invoice','{EDGE_ID}','invoice','{EDGE_ID}','party','{EDGE_ID}','{{\"k\":1}}'); \
             INSERT INTO edge_drift_report VALUES ('r1', now(), 5, 1, false); \
             INSERT INTO edge_drift_entry (report_key, ordinal, kind, edge_type, owner_entity, owner_id, from_entity, from_id, to_entity, to_id) \
                 VALUES ('r1', 0, 'missing', 't','invoice','{EDGE_ID}','invoice','{EDGE_ID}','party','{EDGE_ID}'); \
             INSERT INTO edge_repair_audit VALUES ('r1', 0, now(), 1, 0, 0);"
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn every_table_the_crate_creates_starts_with_spot() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let mut client = db.raw_client().await;
    prepare(&mut client).await.unwrap();
    let all = tables(&client).await;
    assert_eq!(all, {
        let mut want: Vec<String> = NEW.iter().map(|s| (*s).to_owned()).collect();
        want.sort();
        want
    });
    assert!(all.iter().all(|t| t.starts_with("spot_")));
    // The index and the constraints are renamed too.
    let idx = names(
        &client,
        "SELECT indexname::text FROM pg_indexes WHERE schemaname = current_schema() ORDER BY 1",
    )
    .await;
    assert!(idx.iter().all(|i| i.starts_with("spot_")), "{idx:?}");
    assert!(idx.contains(&"spot_graph_edge_owner_idx".to_owned()));
    // Idempotent.
    prepare(&mut client).await.unwrap();
    assert_eq!(tables(&client).await, all);
}

#[tokio::test]
async fn old_tables_are_renamed_with_their_rows_and_constraint_names() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let mut client = db.raw_client().await;
    insert_old_rows(&client).await;
    let before_cons = names(
        &client,
        "SELECT conname::text FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid \
         WHERE t.relname = ANY(ARRAY['graph_edge','edge_drift_report','edge_drift_entry','edge_repair_audit']) ORDER BY 1",
    )
    .await;
    assert!(!before_cons.is_empty());

    prepare(&mut client).await.unwrap();

    let all = tables(&client).await;
    for old in OLD {
        assert!(!all.contains(&old.to_owned()), "{old} still exists");
    }
    for new in NEW {
        assert!(all.contains(&new.to_owned()), "{new} missing");
    }
    for (table, rows) in [
        ("spot_graph_edge", 1_i64),
        ("spot_edge_drift_report", 1),
        ("spot_edge_drift_entry", 1),
        ("spot_edge_repair_audit", 1),
    ] {
        let n: i64 = client
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, rows, "{table}");
    }
    let after_cons = names(
        &client,
        "SELECT conname::text FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid \
         WHERE t.relname LIKE 'spot\\_%' ORDER BY 1",
    )
    .await;
    let expected: Vec<String> = {
        let mut v: Vec<String> = before_cons.iter().map(|c| format!("spot_{c}")).collect();
        v.sort();
        v
    };
    assert_eq!(after_cons, expected);
    let idx = names(
        &client,
        "SELECT indexname::text FROM pg_indexes WHERE schemaname = current_schema() ORDER BY 1",
    )
    .await;
    assert!(idx.iter().all(|i| i.starts_with("spot_")), "{idx:?}");

    // The upgrade is idempotent and keeps the rows.
    prepare(&mut client).await.unwrap();
    let n: i64 = client
        .query_one("SELECT count(*) FROM spot_graph_edge", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 1);
}

#[tokio::test]
async fn both_names_present_is_refused_and_nothing_is_dropped_or_renamed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let mut client = db.raw_client().await;
    insert_old_rows(&client).await;
    // The new name of one pair also exists.
    client
        .batch_execute("CREATE TABLE spot_edge_repair_audit (x int)")
        .await
        .unwrap();
    let before = tables(&client).await;

    let err = prepare(&mut client).await.unwrap_err();
    assert_eq!(err.code(), "edge_pg.migration_failed");
    let message = err.to_string();
    assert!(
        message.contains("`edge_repair_audit`") && message.contains("`spot_edge_repair_audit`")
    );
    assert_eq!(tables(&client).await, before, "nothing renamed or dropped");
    let n: i64 = client
        .query_one("SELECT count(*) FROM graph_edge", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 1);
}

#[tokio::test]
async fn a_foreign_table_with_an_old_name_is_not_renamed() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let mut client = db.raw_client().await;
    client
        .batch_execute("CREATE TABLE graph_edge (a int, b int)")
        .await
        .unwrap();
    let err = prepare(&mut client).await.unwrap_err();
    assert_eq!(err.code(), "edge_pg.migration_failed");
    assert_eq!(tables(&client).await, vec!["graph_edge".to_owned()]);
}
