//! ADR-045: every filter on a hot path is covered by an index, verified from the query plan.
//!
//! The statements are the store's own (`spot_jobs_pg::sql`), run through `EXPLAIN` on a table
//! big enough that the planner would otherwise scan it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_jobs_pg::{PgJobStore, sql};
use spot_postgres::testing::TestDb;
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

async fn plan(client: &Client, statement: &str, params: &[&(dyn ToSql + Sync)]) -> String {
    client
        .query(&format!("EXPLAIN {statement}"), params)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_hot_paths_use_their_indexes() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = PgJobStore::new(common::pool(&db, 4, "plans"));
    store.prepare().await.unwrap();
    let c = db.raw_client().await;
    // Autovacuum is on by default in the throwaway database and this insert crosses its analyze
    // threshold at once, so a background ANALYZE can land between the VACUUM ANALYZE below and
    // the EXPLAINs and the planner would read a different sample than the one this test set up.
    c.batch_execute("ALTER TABLE spot_job SET (autovacuum_enabled = false)")
        .await
        .unwrap();
    // The plan must be the same on a busy shared server: this connection only, no parallel nodes.
    c.batch_execute("SET max_parallel_workers_per_gather = 0")
        .await
        .unwrap();
    // JIT compilation is pure machine-dependent noise in an `EXPLAIN` run: this connection only.
    c.batch_execute("SET jit = off").await.unwrap();
    // 60,000 jobs: mostly finished, some pending, some running, with idempotency keys.
    c.batch_execute(
        "INSERT INTO spot_job (site, name, args, queue, state, max_attempts, not_before,
                               idempotency_key, claim_token, claim_worker, lease_expires_at,
                               finished_at)
         SELECT 'site-' || (g % 17), 'app:work', '{}', 'default',
                CASE WHEN g % 20 < 14 THEN 'succeeded' WHEN g % 20 < 18 THEN 'pending'
                     WHEN g % 20 = 18 THEN 'dead' ELSE 'running' END,
                3, now() - interval '1 hour',
                CASE WHEN g % 20 IN (14, 15, 16, 17, 19) THEN 'key-' || g END,
                CASE WHEN g % 20 = 19 THEN g END,
                CASE WHEN g % 20 = 19 THEN 'w' END,
                CASE WHEN g % 20 = 19 THEN now() + interval '1 hour' END,
                CASE WHEN g % 20 < 14 OR g % 20 = 18 THEN now() - interval '100 days' END
         FROM generate_series(1, 60000) g;",
    )
    .await
    .unwrap();
    // Statistics and the visibility map, as a live table has them (index-only counts).
    c.batch_execute("VACUUM ANALYZE spot_job").await.unwrap();

    let queue = "default".to_owned();
    let worker = "w".to_owned();
    let (ttl, limit, cap) = (30_000_i64, 5_i64, 3_i64);

    let p = plan(&c, sql::CLAIM, &[&queue, &worker, &ttl, &limit]).await;
    println!("claim:\n{p}\n");
    assert!(p.contains("spot_job_claim_idx"), "claim plan:\n{p}");
    assert!(!p.contains("Seq Scan on spot_job "), "claim plan:\n{p}");
    assert!(
        !p.contains("Sort"),
        "the index delivers the claim order:\n{p}"
    );

    let p = plan(
        &c,
        sql::CLAIM_CAPPED,
        &[&queue, &worker, &ttl, &limit, &cap],
    )
    .await;
    println!("capped claim:\n{p}\n");
    assert!(p.contains("spot_job_claim_idx"), "capped claim plan:\n{p}");
    assert!(
        p.contains("spot_job_site_running_idx"),
        "cap count plan:\n{p}"
    );
    assert!(!p.contains("Seq Scan"), "capped claim plan:\n{p}");

    let (site, name, key) = (
        "site-3".to_owned(),
        "app:work".to_owned(),
        "key-1234".to_owned(),
    );
    let p = plan(&c, sql::IDEMPOTENCY_LIVE, &[&site, &name, &key]).await;
    println!("idempotency:\n{p}\n");
    assert!(p.contains("spot_job_idem_idx"), "idempotency plan:\n{p}");

    let token = 19_i64;
    let p = plan(&c, sql::HEARTBEAT, &[&token, &worker, &ttl]).await;
    println!("heartbeat:\n{p}\n");
    assert!(p.contains("spot_job_token_idx"), "heartbeat plan:\n{p}");

    let state = "succeeded".to_owned();
    let none: Option<time::OffsetDateTime> = None;
    let no_error: Option<String> = None;
    let p = plan(
        &c,
        sql::COMPLETE,
        &[&token, &worker, &state, &none, &no_error],
    )
    .await;
    println!("complete:\n{p}\n");
    assert!(p.contains("spot_job_token_idx"), "complete plan:\n{p}");

    let (delay, batch) = (5_000_i64, 500_i64);
    let p = plan(&c, sql::REAP, &[&none, &delay, &batch]).await;
    println!("reap:\n{p}\n");
    assert!(p.contains("spot_job_lease_idx"), "reap plan:\n{p}");

    let states = vec!["dead".to_owned()];
    let age = 90_i64 * 24 * 3_600_000;
    let p = plan(&c, sql::PURGE, &[&states, &age, &batch]).await;
    println!("purge:\n{p}\n");
    assert!(p.contains("spot_job_finished_idx"), "purge plan:\n{p}");

    let p = plan(&c, sql::SITE_RUNNING, &[&site]).await;
    assert!(
        p.contains("spot_job_site_running_idx"),
        "site running plan:\n{p}"
    );
}
