//! The readers and the scan against a real PostgreSQL database.
//!
//! The feed tables are written directly, with chosen positions and publication ages, so a test
//! controls exactly what the capture invariant sees. Every test skips cleanly (printing
//! `SKIPPED:`) without `SPOT_TEST_PG_URL`; with `SPOT_REQUIRE_PG=1` a missing server fails.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::Pool;
use spot_capture_invariant::{Exclusions, Finding, FindingKind};
use spot_capture_invariant_pg::{
    CaptureGate, CapturePgError, Scan, ScanConfig, Snapshot, Watermarks, scan, schema,
};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation, StorageBackend};
use tokio_postgres::Client;

struct Env {
    db: TestDb,
    pool: Pool,
    client: Client,
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap();
    // The feed tables come from the document backend's own `prepare`.
    backend.prepare(&catalog).await.unwrap();
    let client = db.raw_client().await;
    client.batch_execute(schema::DDL).await.unwrap();
    let pool = backend.pool();
    Some(Env { db, pool, client })
}

/// Feed entries `from..=to` (position = i, version 1, insert) published `age` seconds ago.
async fn feed(c: &Client, from: i64, to: i64, age: i64) {
    c.batch_execute(&format!(
        "INSERT INTO spot_change (tx, entity, doc, op, version, captured_at, xid) \
         SELECT md5('tx'||i)::uuid, 'customer', md5('doc'||i)::uuid, 'I', 1, now(), pg_current_xact_id() \
         FROM generate_series({from}::bigint, {to}::bigint) i; \
         INSERT INTO spot_change_pub (generation, seq, tx, entity, doc, published_at) \
         SELECT 1, i, md5('tx'||i)::uuid, 'customer', md5('doc'||i)::uuid, now() - ({age} * interval '1 second') \
         FROM generate_series({from}::bigint, {to}::bigint) i"
    ))
    .await
    .unwrap();
}

/// `doc_state` events for documents `from..=to` at `version` and `op`, published `age` seconds
/// ago, at position `i + shift`.
async fn events(c: &Client, from: i64, to: i64, age: i64, version: i64, op: &str, shift: i64) {
    c.batch_execute(&format!(
        "INSERT INTO spot_outbox (id, tx, ordinal, topic, subject_entity, subject_id, \
                subject_version, schema_version, payload, captured_at, xid) \
         SELECT md5('ev{version}{op}'||i)::uuid, md5('evtx{version}{op}'||i)::uuid, 0, 'doc_state', 'customer', \
                md5('doc'||i)::uuid, {version}, 1, '{{\"data\":{{\"op\":\"{op}\"}}}}', now(), pg_current_xact_id() \
         FROM generate_series({from}::bigint, {to}::bigint) i; \
         INSERT INTO spot_outbox_pub (generation, seq, event_id, published_at) \
         SELECT 1, i + {shift}, md5('ev{version}{op}'||i)::uuid, now() - ({age} * interval '1 second') \
         FROM generate_series({from}::bigint, {to}::bigint) i"
    ))
    .await
    .unwrap();
}

fn cfg(lag: u64) -> ScanConfig {
    ScanConfig {
        page_size: 500,
        window: 50_000,
        lag_secs: lag,
    }
}

fn keys(findings: &[Finding]) -> Vec<(FindingKind, String, u64)> {
    let mut out: Vec<_> = findings
        .iter()
        .map(|f| (f.kind, f.id.clone(), f.version))
        .collect();
    out.sort();
    out
}

async fn run(e: &Env, from: Watermarks, lag: u64) -> Scan {
    scan(&e.pool, Exclusions::new(), from, &cfg(lag))
        .await
        .unwrap()
}

#[tokio::test]
async fn ten_thousand_entries_page_through_and_match() {
    let Some(e) = env().await else { return };
    feed(&e.client, 1, 10_000, 3600).await;
    events(&e.client, 1, 10_000, 3600, 1, "insert", 0).await;
    let s = run(&e, Watermarks::none(), 30).await;
    assert!(s.report.is_clean(), "{:?}", s.report.findings);
    assert_eq!(s.report.feed_entries, 10_000);
    assert_eq!(s.report.state_events, 10_000);
    assert_eq!(s.report.matched, 10_000);
    assert!(s.raise.is_empty());
    assert_eq!((s.next.feed, s.next.events), (10_000, 10_000));
    // The memory bound: the pass never tracked more than the window.
    assert!(s.report.peak_tracked <= 50_000);
}

#[tokio::test]
async fn a_feed_entry_without_an_event_is_a_missing_state_event() {
    let Some(e) = env().await else { return };
    feed(&e.client, 1, 10, 3600).await;
    events(&e.client, 1, 6, 3600, 1, "insert", 0).await;
    events(&e.client, 8, 10, 3600, 1, "insert", 0).await;
    let s = run(&e, Watermarks::none(), 30).await;
    assert_eq!(s.raise.len(), 1);
    assert_eq!(s.raise[0].kind, FindingKind::MissingStateEvent);
    assert_eq!(s.raise[0].entity, "customer");
    // The watermark is held just below the bypassed write on the feed, and passes on the events.
    assert_eq!(s.next.feed, 6);
    assert_eq!(s.next.events, 10);
}

#[tokio::test]
async fn an_event_without_a_feed_entry_is_an_orphan() {
    let Some(e) = env().await else { return };
    feed(&e.client, 1, 5, 3600).await;
    events(&e.client, 1, 6, 3600, 1, "insert", 0).await;
    let s = run(&e, Watermarks::none(), 30).await;
    assert_eq!(s.raise.len(), 1);
    assert_eq!(s.raise[0].kind, FindingKind::OrphanStateEvent);
    assert_eq!(s.next.events, 5);
}

#[tokio::test]
async fn a_version_mismatch_is_reported_once_for_the_document() {
    let Some(e) = env().await else { return };
    feed(&e.client, 1, 3, 3600).await;
    events(&e.client, 1, 2, 3600, 1, "insert", 0).await;
    events(&e.client, 3, 3, 3600, 2, "insert", 0).await;
    let s = run(&e, Watermarks::none(), 30).await;
    assert_eq!(s.raise.len(), 1, "{:?}", s.raise);
    assert_eq!(s.raise[0].kind, FindingKind::VersionMismatch);
}

#[tokio::test]
async fn an_entry_younger_than_the_lag_is_deferred_not_raised() {
    let Some(e) = env().await else { return };
    feed(&e.client, 1, 5, 3600).await;
    events(&e.client, 1, 5, 3600, 1, "insert", 0).await;
    // The feed side of a write published a moment ago, its event not yet.
    feed(&e.client, 6, 6, 1).await;
    let s = run(&e, Watermarks::none(), 30).await;
    assert!(s.raise.is_empty());
    assert_eq!(s.deferred, 1);
    assert_eq!(
        s.next.feed, 5,
        "the watermark never passes an unsettled entry"
    );
    // The event arrives; the next pass resumes and finds nothing at all.
    events(&e.client, 6, 6, 1, 1, "insert", 0).await;
    let s = run(&e, s.next, 30).await;
    assert!(s.raise.is_empty());
    assert_eq!(s.deferred, 0);
}

#[tokio::test]
async fn resuming_from_a_watermark_gives_the_findings_of_one_pass() {
    let Some(e) = env().await else { return };
    // First batch: documents 1..=300, document 150 bypassed.
    feed(&e.client, 1, 300, 7200).await;
    events(&e.client, 1, 149, 7200, 1, "insert", 0).await;
    events(&e.client, 151, 300, 7200, 1, "insert", 0).await;
    let first = run(&e, Watermarks::none(), 30).await;
    assert_eq!(first.raise.len(), 1);
    first.next.save(&e.pool).await.unwrap();

    // The host restarts: the watermark is read back from the table.
    let restored = Watermarks::load(&e.pool).await.unwrap();
    assert_eq!(restored, first.next);

    // Second batch: documents 301..=600, document 450 has an orphan twin (event only).
    feed(&e.client, 301, 600, 3600).await;
    events(&e.client, 301, 600, 3600, 1, "insert", 0).await;
    events(&e.client, 450, 450, 3500, 7, "update", 1_000).await;
    let resumed = run(&e, restored, 30).await;
    // One pass over everything, from nothing.
    let whole = run(&e, Watermarks::none(), 30).await;
    assert_eq!(keys(&resumed.raise), keys(&whole.raise));
    assert_eq!(whole.raise.len(), 2);
    // The bypassed write still holds the feed watermark; the orphan holds the events one.
    assert_eq!(resumed.next.feed, 149);
}

#[tokio::test]
async fn a_new_generation_discards_the_old_watermark() {
    let Some(e) = env().await else { return };
    feed(&e.client, 1, 3, 3600).await;
    events(&e.client, 1, 3, 3600, 1, "insert", 0).await;
    let stale = Watermarks {
        generation: 41,
        feed: 3,
        events: 3,
    };
    let s = run(&e, stale, 30).await;
    assert_eq!(
        s.from,
        Watermarks {
            generation: 1,
            feed: 0,
            events: 0
        }
    );
    assert_eq!(s.report.matched, 3);
}

#[tokio::test]
async fn an_excluded_entity_is_skipped_on_both_sides() {
    let Some(e) = env().await else { return };
    feed(&e.client, 1, 3, 3600).await;
    let ex = Exclusions::new()
        .local("customer", "platform", "local_only")
        .unwrap();
    let s = scan(&e.pool, ex, Watermarks::none(), &cfg(30))
        .await
        .unwrap();
    assert!(s.raise.is_empty());
    assert_eq!(s.report.exclusions[0].feed_entries_skipped, 3);
}

#[tokio::test]
async fn the_gate_reads_everything_with_no_lag() {
    let Some(e) = env().await else { return };
    feed(&e.client, 1, 4, 0).await;
    events(&e.client, 1, 3, 0, 1, "insert", 0).await;
    let report = CaptureGate::new().run_once(&e.pool).await.unwrap();
    assert_eq!(report.count(FindingKind::MissingStateEvent), 1);
    events(&e.client, 4, 4, 0, 1, "insert", 0).await;
    assert!(
        CaptureGate::new()
            .run_once(&e.pool)
            .await
            .unwrap()
            .is_clean()
    );
}

#[tokio::test]
async fn options_must_be_positive() {
    let Some(e) = env().await else { return };
    let bad = ScanConfig {
        page_size: 0,
        ..cfg(30)
    };
    let err = scan(&e.pool, Exclusions::new(), Watermarks::none(), &bad)
        .await
        .unwrap_err();
    assert_eq!(err, CapturePgError::OptionsInvalid);
}

#[tokio::test]
async fn the_snapshot_cannot_write_and_a_select_only_role_can_run_a_pass() {
    let Some(e) = env().await else { return };
    feed(&e.client, 1, 3, 3600).await;
    events(&e.client, 1, 3, 3600, 1, "insert", 0).await;
    let role = format!(
        "cap_ro_{}",
        e.db.name().replace("spot_test_", "").replace('-', "_")
    );
    e.client
        .batch_execute(&format!(
            "CREATE ROLE {role} LOGIN PASSWORD 'ro'; \
             GRANT SELECT ON spot_meta, spot_change, spot_change_pub, spot_outbox, spot_outbox_pub, \
                 spot_capture_watermark TO {role}"
        ))
        .await
        .unwrap();
    let mut config = e.db.config();
    config.user = role.clone();
    config.password = "ro".to_owned();
    let backend = PgBackend::connect_unchecked(config).unwrap();
    let pool = backend.pool();

    // A role with SELECT alone reads and compares: the pass needs nothing else.
    let s = scan(&pool, Exclusions::new(), Watermarks::none(), &cfg(30))
        .await
        .unwrap();
    assert!(s.report.is_clean());

    // And it cannot write the feed or the outbox, snapshot or not.
    let conn = pool.get().await.unwrap();
    assert!(
        conn.batch_execute("DELETE FROM spot_change_pub")
            .await
            .is_err(),
        "the role must not be able to change the feed"
    );
    assert!(
        conn.batch_execute("DELETE FROM spot_outbox_pub")
            .await
            .is_err()
    );
    drop(conn);

    // The snapshot itself is read-only whatever the role may do.
    let snapshot = Snapshot::open(&pool).await.unwrap();
    let heads = snapshot.heads(30).await.unwrap();
    assert_eq!(heads.feed, 3);
    assert_eq!(heads.events, 3);
    snapshot.finish().await;

    let count: i64 = e
        .client
        .query_one("SELECT count(*) FROM spot_change_pub", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 3);
    drop(pool);
    drop(backend);
    let _ = e
        .client
        .batch_execute(&format!(
            "REVOKE ALL ON spot_meta, spot_change, spot_change_pub, spot_outbox, spot_outbox_pub, \
                 spot_capture_watermark FROM {role}; DROP ROLE {role}"
        ))
        .await;
}
