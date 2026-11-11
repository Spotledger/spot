//! ADR-045: the two named lookups — "Cases waiting on event X" and "timers due" — are covered by
//! their index, verified from the query plan on a table big enough to matter.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bpm_pg::{PgCaseStore, sql};
use spot_postgres::testing::TestDb;
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

async fn explain(client: &Client, statement: &str, params: &[&(dyn ToSql + Sync)]) -> String {
    client
        .query(&format!("EXPLAIN {statement}"), params)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wait_and_timer_lookups_use_their_indexes() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = PgCaseStore::new(pool(&db, 4, "bpm-plans"));
    store.prepare().await.unwrap();
    let client = db.raw_client().await;

    // 60,000 node rows; 60 wait on one event, 60 have a due timer, the rest neither.
    client
        .batch_execute(
            "INSERT INTO spot_bpm_node_outcome \
                 (case_id, tenant, node, attempt, state, started_at, event, due) \
             SELECT 'case_' || g, 'default', 'n0', 1, 'waiting', now(), \
                    CASE WHEN g % 1000 = 0 THEN 'order.approved' END, \
                    CASE WHEN g % 1000 = 1 THEN now() - interval '1 hour' END \
             FROM generate_series(1, 60000) g;",
        )
        .await
        .unwrap();
    client
        .batch_execute("VACUUM ANALYZE spot_bpm_node_outcome")
        .await
        .unwrap();

    let event = "order.approved".to_owned();
    let p = explain(&client, sql::WAITING_ON_EVENT, &[&"default", &event]).await;
    println!("waiting on event:\n{p}\n");
    assert!(
        p.contains("spot_bpm_node_outcome_event_idx"),
        "event plan:\n{p}"
    );
    assert!(
        !p.contains("Seq Scan on spot_bpm_node_outcome"),
        "event plan:\n{p}"
    );

    let p = explain(&client, sql::DUE_TIMERS, &[&now(), &"default"]).await;
    println!("timers due:\n{p}\n");
    assert!(
        p.contains("spot_bpm_node_outcome_due_idx"),
        "due plan:\n{p}"
    );
    assert!(
        !p.contains("Seq Scan on spot_bpm_node_outcome"),
        "due plan:\n{p}"
    );
}
