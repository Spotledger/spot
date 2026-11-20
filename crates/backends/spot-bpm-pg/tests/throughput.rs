//! `PgThroughput` (WP-BPM-11.T10): completed Cases per process version per UTC ISO week, bounded
//! and indexed. Real PostgreSQL through `TestDb`; skips without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::pool;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_pg::throughput::WEEKLY;
use spot_bpm_pg::{PgCaseStore, PgThroughput, WeeklyRow};
use spot_postgres::testing::TestDb;
use tokio_postgres::Client;

/// A scratch database with the store's tables and the throughput index.
async fn setup(app: &str) -> Option<(TestDb, PgThroughput, Client)> {
    let db = TestDb::create().await?;
    let pool = pool(&db, 4, app);
    PgCaseStore::new(pool.clone()).prepare().await.unwrap();
    let throughput = PgThroughput::new(pool);
    throughput.prepare().await.unwrap();
    let client = db.raw_client().await;
    Some((db, throughput, client))
}

/// An instant from its text form, parsed by the database so the test needs no date library.
async fn ts(client: &Client, text: &str) -> OffsetDateTime {
    client
        .query_one("SELECT $1::text::timestamptz", &[&text])
        .await
        .unwrap()
        .get(0)
}

/// Inserts one Case row directly (the throughput query only reads these columns).
async fn insert(
    client: &Client,
    tenant: &str,
    case: &str,
    process: &str,
    version: i32,
    status: &str,
    ended: Option<&str>,
) {
    client
        .execute(
            "INSERT INTO spot_bpm_case \
                 (case_id, tenant, business_key, dag_key, dag_version, content_hash, status, \
                  version, started_at, ended_at, nodes) \
             VALUES ($1, $2, $1, $3, $4, 'h', $5, 0, now(), $6::text::timestamptz, '{}'::jsonb)",
            &[&case, &tenant, &process, &version, &status, &ended],
        )
        .await
        .unwrap();
}

fn row(process: &str, version: u32, week: &str, completed: u64) -> WeeklyRow {
    WeeklyRow {
        process: process.to_owned(),
        version,
        week: week.to_owned(),
        completed,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cases_are_counted_per_week_and_version_for_the_right_tenant_only() {
    let Some((_db, throughput, client)) = setup("bpm-tp-count").await else {
        return;
    };
    // 2026-11-03 is in ISO week 45, 2026-11-10 in week 46 and 2026-11-17 in week 47.
    insert(
        &client,
        "default",
        "a1",
        "order",
        1,
        "completed",
        Some("2026-11-03T10:00:00Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "a2",
        "order",
        1,
        "completed",
        Some("2026-11-04T10:00:00Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "a3",
        "order",
        2,
        "completed",
        Some("2026-11-04T11:00:00Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "a4",
        "order",
        2,
        "completed",
        Some("2026-11-10T09:00:00Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "a5",
        "refund",
        1,
        "completed",
        Some("2026-11-17T09:00:00Z"),
    )
    .await;
    insert(
        &client,
        "other",
        "b1",
        "order",
        1,
        "completed",
        Some("2026-11-03T10:00:00Z"),
    )
    .await;
    insert(
        &client,
        "other",
        "b2",
        "order",
        1,
        "completed",
        Some("2026-11-10T10:00:00Z"),
    )
    .await;

    let since = ts(&client, "2026-11-01T00:00:00Z").await;
    let until = ts(&client, "2026-12-01T00:00:00Z").await;
    let rows = throughput.weekly(since, until, 100).await.unwrap();
    assert_eq!(
        rows,
        vec![
            row("order", 1, "2026-W45", 2),
            row("order", 2, "2026-W45", 1),
            row("order", 2, "2026-W46", 1),
            row("refund", 1, "2026-W47", 1),
        ]
    );

    let other = throughput.clone().with_tenant("other");
    assert_eq!(other.tenant(), "other");
    let rows = other.weekly(since, until, 100).await.unwrap();
    assert_eq!(
        rows,
        vec![
            row("order", 1, "2026-W45", 1),
            row("order", 1, "2026-W46", 1)
        ]
    );

    let nobody = throughput.with_tenant("nobody");
    assert!(nobody.weekly(since, until, 100).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_completed_cases_count() {
    let Some((_db, throughput, client)) = setup("bpm-tp-status").await else {
        return;
    };
    insert(
        &client,
        "default",
        "c1",
        "order",
        1,
        "completed",
        Some("2026-11-03T10:00:00Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "c2",
        "order",
        1,
        "failed",
        Some("2026-11-03T10:00:00Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "c3",
        "order",
        1,
        "cancelled",
        Some("2026-11-03T10:00:00Z"),
    )
    .await;
    insert(&client, "default", "c4", "order", 1, "running", None).await;
    let since = ts(&client, "2026-11-01T00:00:00Z").await;
    let until = ts(&client, "2026-12-01T00:00:00Z").await;
    assert_eq!(
        throughput.weekly(since, until, 100).await.unwrap(),
        vec![row("order", 1, "2026-W45", 1)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_week_is_the_iso_week_of_the_iso_year_in_utc() {
    let Some((_db, throughput, client)) = setup("bpm-tp-iso").await else {
        return;
    };
    // 2026 has 53 ISO weeks: 2027-01-01 (a Friday) is still in 2026-W53.
    insert(
        &client,
        "default",
        "i1",
        "p",
        1,
        "completed",
        Some("2026-12-28T00:00:00Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "i2",
        "p",
        1,
        "completed",
        Some("2026-12-31T23:59:59Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "i3",
        "p",
        1,
        "completed",
        Some("2027-01-01T00:00:00Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "i4",
        "p",
        1,
        "completed",
        Some("2027-01-04T00:00:00Z"),
    )
    .await;
    // A non-UTC offset is converted first: 23:30 at -05:00 on 2027-01-03 is Monday 2027-01-04
    // 04:30 UTC, which is week 1 of 2027.
    insert(
        &client,
        "default",
        "i5",
        "p",
        1,
        "completed",
        Some("2027-01-03T23:30:00-05:00"),
    )
    .await;
    let since = ts(&client, "2026-12-01T00:00:00Z").await;
    let until = ts(&client, "2027-02-01T00:00:00Z").await;
    assert_eq!(
        throughput.weekly(since, until, 100).await.unwrap(),
        vec![row("p", 1, "2026-W53", 3), row("p", 1, "2027-W01", 2)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn since_is_inclusive_and_until_exclusive_to_the_microsecond() {
    let Some((_db, throughput, client)) = setup("bpm-tp-bounds").await else {
        return;
    };
    insert(
        &client,
        "default",
        "m1",
        "p",
        1,
        "completed",
        Some("2026-11-03T10:00:00.000000Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "m2",
        "p",
        1,
        "completed",
        Some("2026-11-03T10:00:00.000001Z"),
    )
    .await;
    insert(
        &client,
        "default",
        "m3",
        "p",
        1,
        "completed",
        Some("2026-11-03T10:00:00.000002Z"),
    )
    .await;
    let t0 = ts(&client, "2026-11-03T10:00:00.000000Z").await;
    let t1 = ts(&client, "2026-11-03T10:00:00.000001Z").await;
    let t2 = ts(&client, "2026-11-03T10:00:00.000002Z").await;
    let t3 = ts(&client, "2026-11-03T10:00:00.000003Z").await;
    let count = |rows: Vec<WeeklyRow>| rows.iter().map(|r| r.completed).sum::<u64>();
    // [t0, t3): all three.
    assert_eq!(count(throughput.weekly(t0, t3, 10).await.unwrap()), 3);
    // [t1, t3): the first is before `since`, so two.
    assert_eq!(count(throughput.weekly(t1, t3, 10).await.unwrap()), 2);
    // [t0, t2): `until` excludes the row that ended exactly at it, so two.
    assert_eq!(count(throughput.weekly(t0, t2, 10).await.unwrap()), 2);
    // [t1, t2): exactly the one row at `since`.
    assert_eq!(count(throughput.weekly(t1, t2, 10).await.unwrap()), 1);
    // An empty window is empty.
    assert!(throughput.weekly(t1, t1, 10).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_limit_is_clamped_to_one_through_five_thousand() {
    let Some((_db, throughput, client)) = setup("bpm-tp-limit").await else {
        return;
    };
    // 5,100 process names, one completed Case each, in one week: 5,100 groups.
    client
        .batch_execute(
            "INSERT INTO spot_bpm_case \
                 (case_id, tenant, business_key, dag_key, dag_version, content_hash, status, \
                  version, started_at, ended_at, nodes) \
             SELECT 'k' || g, 'default', 'k' || g, 'proc_' || lpad(g::text, 5, '0'), 1, 'h', \
                    'completed', 0, now(), timestamptz '2026-11-03 10:00:00+00', '{}'::jsonb \
             FROM generate_series(1, 5100) g",
        )
        .await
        .unwrap();
    let since = ts(&client, "2026-11-01T00:00:00Z").await;
    let until = ts(&client, "2026-12-01T00:00:00Z").await;

    // Zero is raised to one, and the first row is the first by process name.
    let one = throughput.weekly(since, until, 0).await.unwrap();
    assert_eq!(one, vec![row("proc_00001", 1, "2026-W45", 1)]);
    assert_eq!(throughput.weekly(since, until, 1).await.unwrap(), one);
    assert_eq!(throughput.weekly(since, until, 7).await.unwrap().len(), 7);
    // Anything above 5,000 is cut to 5,000.
    assert_eq!(
        throughput.weekly(since, until, 5000).await.unwrap().len(),
        5000
    );
    assert_eq!(
        throughput.weekly(since, until, 5001).await.unwrap().len(),
        5000
    );
    assert_eq!(
        throughput
            .weekly(since, until, u32::MAX)
            .await
            .unwrap()
            .len(),
        5000
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_twice_is_harmless_and_creates_the_index() {
    let Some((_db, throughput, client)) = setup("bpm-tp-prepare").await else {
        return;
    };
    throughput.prepare().await.unwrap();
    throughput.prepare().await.unwrap();
    let n: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_indexes WHERE indexname = 'spot_bpm_case_ended_idx'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 1);
    // Concurrent prepares from several tasks all succeed.
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let t = throughput.clone();
        tasks.push(tokio::spawn(async move { t.prepare().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_one_month_window_over_five_thousand_rows_uses_the_ended_index() {
    let Some((_db, throughput, client)) = setup("bpm-tp-plan").await else {
        return;
    };
    // 5,000 Cases ending every 105 minutes through 2026 (about a year), with a padded node map
    // so the table spans enough pages for an index scan to beat a sequential one.
    client
        .batch_execute(
            "INSERT INTO spot_bpm_case \
                 (case_id, tenant, business_key, dag_key, dag_version, content_hash, status, \
                  version, started_at, ended_at, nodes) \
             SELECT 'p' || g, 'default', 'p' || g, 'order', 1, 'h', 'completed', 0, now(), \
                    timestamptz '2026-01-01 00:00:00+00' + g * interval '105 minutes', \
                    jsonb_build_object('pad', repeat(md5(g::text), 50)) \
             FROM generate_series(1, 5000) g",
        )
        .await
        .unwrap();
    client.batch_execute("ANALYZE spot_bpm_case").await.unwrap();
    let since = ts(&client, "2026-06-01T00:00:00Z").await;
    let until = ts(&client, "2026-07-01T00:00:00Z").await;
    let limit = 5000_i64;
    let plan = client
        .query(
            &format!("EXPLAIN {WEEKLY}"),
            &[&"default", &since, &until, &limit],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n");
    println!("throughput plan:\n{plan}");
    assert!(plan.contains("spot_bpm_case_ended_idx"), "plan:\n{plan}");
    assert!(!plan.contains("Seq Scan on spot_bpm_case"), "plan:\n{plan}");
    // And the real call over the same window still answers.
    let rows = throughput.weekly(since, until, 100).await.unwrap();
    assert!(!rows.is_empty());
}
