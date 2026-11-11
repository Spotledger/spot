//! Behaviour beyond the contract suite: races, restarts, out-of-range partitions compared with
//! the in-memory store, site isolation, replacement, corruption and hostile names.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{instant, outcome, pos, record, spec, store};
use spot_event_stream::{ConsumerRecord, ConsumerStore, InMemoryConsumerStore, Start};
use spot_event_stream_pg::PgConsumerStore;
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exactly_one_of_racing_advances_wins() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "acme", 16).await;
    store.register(&record("race", 1)).await.unwrap();
    for round in 1..=20u64 {
        let expected = store.checkpoint("race", 0).await.unwrap();
        let mut tasks = Vec::new();
        for writer in 0..8u64 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                let to = pos(1, round * 100 + writer + 1);
                (to, store.advance("race", 0, to, expected).await)
            }));
        }
        let mut winners = Vec::new();
        for task in tasks {
            let (to, result) = task.await.unwrap();
            match result {
                Ok(()) => winners.push(to),
                Err(error) => assert_eq!(error.code(), "eventstream.ack_refused"),
            }
        }
        assert_eq!(winners.len(), 1, "round {round}: {winners:?}");
        assert_eq!(store.checkpoint("race", 0).await.unwrap(), winners[0]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_of_the_store_keeps_checkpoints_and_parking() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let before = store(&db, "acme", 4).await;
    before.register(&record("alpha", 3)).await.unwrap();
    before.register(&record("beta", 2)).await.unwrap();
    before
        .advance("alpha", 0, pos(1, 7), pos(0, 0))
        .await
        .unwrap();
    before
        .advance("alpha", 2, pos(2, 1), pos(0, 0))
        .await
        .unwrap();
    before.park("alpha", 1, instant(5_000)).await.unwrap();
    let listed_before = before.list().await.unwrap();
    drop(before);
    // A new pool and store: what a restarted process sees.
    let after = store(&db, "acme", 4).await;
    assert_eq!(after.list().await.unwrap(), listed_before);
    let alpha = after.get("alpha").await.unwrap().unwrap();
    assert_eq!(alpha.checkpoints, vec![pos(1, 7), pos(0, 0), pos(2, 1)]);
    assert_eq!(alpha.parked, vec![false, true, false]);
    assert_eq!(alpha.parked_since[1], Some(instant(5_000)));
    assert_eq!(alpha.registered_at, instant(1_000));
}

/// One scripted run of the edge cases; the same script runs on both stores.
async fn edge_script(store: Arc<dyn ConsumerStore>) -> Vec<String> {
    let mut out = Vec::new();
    store.register(&record("edge", 2)).await.unwrap();
    for partition in [2u32, 5, u32::MAX] {
        out.push(outcome(store.checkpoint("edge", partition).await));
        out.push(outcome(
            store.advance("edge", partition, pos(1, 1), pos(0, 0)).await,
        ));
        out.push(outcome(store.restart("edge", partition, pos(1, 1)).await));
        out.push(outcome(store.park("edge", partition, instant(1)).await));
        out.push(outcome(store.unpark("edge", partition).await));
    }
    out.push(outcome(store.checkpoint("ghost", 0).await));
    out.push(outcome(store.checkpoint("ghost", 9).await));
    out.push(outcome(
        store.advance("ghost", 0, pos(1, 1), pos(0, 0)).await,
    ));
    out.push(outcome(store.restart("ghost", 0, pos(1, 1)).await));
    out.push(outcome(store.park("ghost", 0, instant(1)).await));
    out.push(outcome(store.unpark("ghost", 0).await));
    out.push(outcome(store.delete("ghost").await));
    // Advance semantics.
    out.push(outcome(
        store.advance("edge", 0, pos(1, 5), pos(0, 0)).await,
    ));
    out.push(outcome(
        store.advance("edge", 0, pos(1, 5), pos(1, 5)).await,
    )); // same: ok
    out.push(outcome(
        store.advance("edge", 0, pos(1, 5), pos(0, 0)).await,
    )); // stale: refused
    out.push(outcome(
        store.advance("edge", 0, pos(1, 2), pos(1, 5)).await,
    )); // backwards
    out.push(outcome(
        store.advance("edge", 0, pos(1, 2), pos(0, 0)).await,
    )); // stale and behind
    out.push(outcome(
        store.advance("edge", 0, pos(1, 9), pos(1, 6)).await,
    )); // wrong expected
    out.push(outcome(
        store.advance("edge", 0, pos(3, 0), pos(1, 5)).await,
    )); // new generation
    out.push(outcome(
        store.advance("edge", 0, pos(3, 0), pos(3, 1)).await,
    )); // same-as-stored, stale expected
    // Parking: first instant is kept, restart clears it.
    out.push(outcome(store.park("edge", 1, instant(10)).await));
    out.push(outcome(store.park("edge", 1, instant(20)).await));
    let loaded = store.get("edge").await.unwrap().unwrap();
    out.push(format!("{:?} {:?}", loaded.parked, loaded.parked_since));
    out.push(outcome(store.restart("edge", 1, pos(0, 0)).await));
    let loaded = store.get("edge").await.unwrap().unwrap();
    out.push(format!(
        "{:?} {:?} {:?}",
        loaded.checkpoints, loaded.parked, loaded.parked_since
    ));
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edge_cases_match_the_in_memory_store() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pg = store(&db, "acme", 4).await;
    let expected = edge_script(Arc::new(InMemoryConsumerStore::new())).await;
    let actual = edge_script(Arc::new(pg)).await;
    assert_eq!(actual, expected);
    // And the script really exercised the error paths.
    assert!(expected.iter().any(|o| o == "eventstream.spec_invalid"));
    assert!(
        expected
            .iter()
            .any(|o| o == "eventstream.consumer_not_found")
    );
    assert!(expected.iter().any(|o| o == "eventstream.ack_refused"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sites_are_isolated_even_under_the_same_consumer_name() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let acme = store(&db, "acme", 4).await;
    let beta = store(&db, "beta", 4).await;
    acme.register(&record("shared", 2)).await.unwrap();
    assert!(beta.get("shared").await.unwrap().is_none());
    assert!(beta.list().await.unwrap().is_empty());
    assert_eq!(
        outcome(beta.advance("shared", 0, pos(1, 1), pos(0, 0)).await),
        "eventstream.consumer_not_found"
    );
    assert_eq!(
        outcome(beta.park("shared", 0, instant(1)).await),
        "eventstream.consumer_not_found"
    );
    beta.register(&record("shared", 3)).await.unwrap();
    acme.advance("shared", 0, pos(1, 4), pos(0, 0))
        .await
        .unwrap();
    assert_eq!(beta.checkpoint("shared", 0).await.unwrap(), pos(0, 0));
    beta.delete("shared").await.unwrap();
    assert!(
        acme.get("shared").await.unwrap().is_some(),
        "delete is per site"
    );
    assert_eq!(acme.checkpoint("shared", 0).await.unwrap(), pos(1, 4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn register_replaces_the_whole_record_like_the_in_memory_store() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pg: Arc<dyn ConsumerStore> = Arc::new(store(&db, "acme", 4).await);
    let memory: Arc<dyn ConsumerStore> = Arc::new(InMemoryConsumerStore::new());
    for store in [&pg, &memory] {
        store.register(&record("swap", 3)).await.unwrap();
        store
            .advance("swap", 1, pos(1, 9), pos(0, 0))
            .await
            .unwrap();
        store.park("swap", 2, instant(77)).await.unwrap();
        // Same record again: replaces, so progress is gone (the engine never does this for an
        // existing consumer; it passes the stored record).
        store.register(&record("swap", 3)).await.unwrap();
        let mut shrunk = record("swap", 2);
        shrunk.checkpoints[1] = pos(4, 4);
        store.register(&shrunk).await.unwrap();
        let mut grown = record("swap", 4);
        grown.spec.start = Start::FromPosition(pos(2, 2));
        grown.checkpoints = grown.spec.initial_positions();
        store.register(&grown).await.unwrap();
    }
    assert_eq!(
        pg.get("swap").await.unwrap(),
        memory.get("swap").await.unwrap()
    );
    assert_eq!(pg.list().await.unwrap(), memory.list().await.unwrap());
    assert_eq!(pg.get("swap").await.unwrap().unwrap().checkpoints.len(), 4);
    // Passing the stored record back keeps its progress.
    pg.advance("swap", 3, pos(5, 5), pos(2, 2)).await.unwrap();
    let stored = pg.get("swap").await.unwrap().unwrap();
    pg.register(&stored).await.unwrap();
    assert_eq!(pg.get("swap").await.unwrap().unwrap(), stored);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn register_refuses_a_record_that_does_not_fit_its_spec() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "acme", 4).await;
    let mut short = record("short", 3);
    short.checkpoints.pop();
    assert_eq!(
        outcome(store.register(&short).await),
        "eventstream.spec_invalid"
    );
    let mut zero = record("zero", 1);
    zero.spec.partitions = 0;
    zero.checkpoints.clear();
    zero.parked.clear();
    zero.parked_since.clear();
    assert_eq!(
        outcome(store.register(&zero).await),
        "eventstream.spec_invalid"
    );
    let mut huge = record("huge", 1);
    huge.checkpoints[0] = pos(u64::MAX, 0);
    assert_eq!(
        outcome(store.register(&huge).await),
        "eventstream.position_invalid"
    );
    assert!(
        store.list().await.unwrap().is_empty(),
        "nothing half-written"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn positions_beyond_bigint_are_refused_not_wrapped() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "acme", 4).await;
    store.register(&record("big", 1)).await.unwrap();
    assert_eq!(
        outcome(store.advance("big", 0, pos(u64::MAX, 0), pos(0, 0)).await),
        "eventstream.position_invalid"
    );
    assert_eq!(
        outcome(store.restart("big", 0, pos(1, u64::MAX)).await),
        "eventstream.position_invalid"
    );
    assert_eq!(store.checkpoint("big", 0).await.unwrap(), pos(0, 0));
    let top = pos(i64::MAX as u64, i64::MAX as u64);
    store.advance("big", 0, top, pos(0, 0)).await.unwrap();
    assert_eq!(store.checkpoint("big", 0).await.unwrap(), top);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_is_in_name_order_and_delete_removes_partitions() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "acme", 4).await;
    for name in ["mu", "alpha", "zeta", "beta"] {
        store.register(&record(name, 2)).await.unwrap();
    }
    let names: Vec<String> = store
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.spec.name)
        .collect();
    assert_eq!(names, ["alpha", "beta", "mu", "zeta"]);
    store.delete("mu").await.unwrap();
    store.delete("mu").await.unwrap(); // deleting twice is fine
    let raw = db.raw_client().await;
    let rows: i64 = raw
        .query_one(
            "SELECT count(*) FROM spot_event_partition WHERE consumer = 'mu'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 0, "partitions went with the consumer");
    assert_eq!(store.list().await.unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_idempotent_and_safe_to_run_concurrently() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = common::pool(&db, 8, "event_stream_prepare");
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let store = PgConsumerStore::new(pool.clone(), common::site("acme"));
        tasks.push(tokio::spawn(async move { store.prepare().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    PgConsumerStore::new(pool, common::site("acme"))
        .prepare()
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_rows_fail_closed_with_a_storage_code() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "acme", 4).await;
    let raw = db.raw_client().await;
    store.register(&record("one", 3)).await.unwrap();
    raw.execute(
        "DELETE FROM spot_event_partition WHERE consumer = 'one' AND part = 1",
        &[],
    )
    .await
    .unwrap();
    let error = store.get("one").await.unwrap_err();
    assert!(error.code().starts_with("storage."), "{}", error.code());
    assert!(store.list().await.is_err());
    store.register(&record("two", 1)).await.unwrap();
    raw.execute(
        "UPDATE spot_event_consumer SET spec = '{\"nonsense\": true}'::jsonb WHERE name = 'two'",
        &[],
    )
    .await
    .unwrap();
    assert!(store.get("two").await.is_err());
    raw.execute(
        "UPDATE spot_event_consumer SET spec = jsonb_set(spec, '{name}', '\"other\"') WHERE name = 'two'",
        &[],
    )
    .await
    .unwrap();
    assert!(
        store.get("two").await.is_err(),
        "a definition naming another consumer"
    );
    // The error text names no value.
    let text = format!("{}", store.get("two").await.unwrap_err());
    assert!(
        !text.contains("nonsense") && !text.contains("other"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_names_are_data_not_sql() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "acme", 4).await;
    let evil = "x'; DROP TABLE spot_event_consumer; --";
    let mut rec = ConsumerRecord::new(spec(evil, 1), instant(1));
    rec.spec.owner = "o'wner".to_owned();
    store.register(&rec).await.unwrap();
    assert_eq!(store.get(evil).await.unwrap().unwrap(), rec);
    store.advance(evil, 0, pos(1, 1), pos(0, 0)).await.unwrap();
    assert_eq!(store.list().await.unwrap().len(), 1);
    store.delete(evil).await.unwrap();
    assert!(store.list().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_database_is_a_storage_error_not_a_panic() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "acme", 2).await;
    let raw = db.raw_client().await;
    drop(raw);
    // A pool on a database name that does not exist.
    let mut cfg = deadpool_postgres::Config::new();
    let c = db.config();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some("spot_test_does_not_exist".to_owned());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    let broken = PgConsumerStore::new(
        cfg.create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap(),
        common::site("acme"),
    );
    let error = broken.list().await.unwrap_err();
    assert!(error.code().starts_with("storage."), "{}", error.code());
    assert!(
        !format!("{error}").contains(&c.password),
        "no password in the error"
    );
    assert!(broken.prepare().await.is_err());
    let _ = store;
}
