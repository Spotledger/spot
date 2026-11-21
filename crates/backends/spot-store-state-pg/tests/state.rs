//! The PostgreSQL store state against a real server (`SPOT_TEST_PG_URL`; skipped with a message
//! when unset, a failure when `SPOT_REQUIRE_PG=1`). The shared conformance cases run here and
//! against the memory double, so the two are held to one contract.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Barrier};
use std::time::Duration;

use common::conformance::*;
use common::*;
use spot_pg_bridge::Bridge;
use spot_store_state::*;
use spot_store_state_pg::PgStoreState;

const GRACE: Duration = Duration::from_secs(2);

#[test]
fn the_postgres_store_passes_the_shared_cases() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    run_all(&*store);
    bridge.shutdown(GRACE).unwrap();
}

#[test]
fn the_migration_runs_twice_and_keeps_the_data() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let id = store
        .issue_publisher_key("mig_pub", hash(1), "k", 1)
        .unwrap();
    let first = store.next_generation(1).unwrap();
    store.prepare().unwrap();
    store.prepare().unwrap();
    let (b2, second) = open_db(fx.db.config());
    assert_eq!(
        second
            .lookup_publisher_key(&hash(1))
            .unwrap()
            .unwrap()
            .key_id,
        id
    );
    assert_eq!(
        second.next_generation(2).unwrap(),
        first + 1,
        "the counter is not reset"
    );
    b2.shutdown(GRACE).unwrap();
    bridge.shutdown(GRACE).unwrap();
}

/// Two processes: two bridges, two pools, one database.
type Opened = (Arc<Bridge>, Arc<PgStoreState>);

fn two_stores(fx: &Fixture) -> (Opened, Opened) {
    (open_db(fx.db.config()), open_db(fx.db.config()))
}

#[test]
fn racing_generations_are_distinct_and_increasing_across_connections() {
    let Some(fx) = fixture() else { return };
    let ((b1, s1), (b2, s2)) = two_stores(&fx);
    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for store in [&s1, &s2, &s1, &s2] {
        let (store, barrier) = (Arc::clone(store), Arc::clone(&barrier));
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            (0..25)
                .map(|_| store.next_generation(1).unwrap())
                .collect::<Vec<u64>>()
        }));
    }
    let mut all = Vec::new();
    for handle in handles {
        let own = handle.join().unwrap();
        assert!(own.windows(2).all(|w| w[0] < w[1]), "increasing per caller");
        all.extend(own);
    }
    all.sort_unstable();
    let before = all.len();
    all.dedup();
    assert_eq!(all.len(), before, "no value handed out twice");
    assert_eq!(all, (1..=100).collect::<Vec<u64>>(), "and none skipped");
    b1.shutdown(GRACE).unwrap();
    b2.shutdown(GRACE).unwrap();
}

#[test]
fn racing_listing_decisions_have_exactly_one_winner() {
    let Some(fx) = fixture() else { return };
    let ((b1, s1), (b2, s2)) = two_stores(&fx);
    for round in 0..5 {
        let name = app(&format!("race_{round}"));
        let v = version("1.0.0");
        s1.set_listing(&name, &v, ListingState::PendingReview, "a", 1)
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let spawn = |store: &Arc<PgStoreState>, to: ListingState| {
            let (store, barrier) = (Arc::clone(store), Arc::clone(&barrier));
            let (name, v) = (name.clone(), v.clone());
            std::thread::spawn(move || {
                barrier.wait();
                store.set_listing(&name, &v, to, "racer", 2)
            })
        };
        let listed = spawn(&s1, ListingState::Listed);
        let rejected = spawn(&s2, ListingState::Rejected);
        let (listed, rejected) = (listed.join().unwrap(), rejected.join().unwrap());
        assert_eq!(
            listed.is_ok() as u8 + rejected.is_ok() as u8,
            1,
            "exactly one wins"
        );
        let loser = listed.err().or(rejected.err()).unwrap();
        assert_eq!(loser.code, BAD_TRANSITION);
        let state = s1.listing(&name, &v).unwrap().unwrap().state;
        assert_eq!(
            state == ListingState::Listed,
            s2.listed_among(&[(name, v)]).unwrap().len() == 1
        );
    }
    b1.shutdown(GRACE).unwrap();
    b2.shutdown(GRACE).unwrap();
}

#[test]
fn racing_idempotency_claims_see_one_new() {
    let Some(fx) = fixture() else { return };
    let ((b1, s1), (b2, s2)) = two_stores(&fx);
    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for store in [&s1, &s2, &s1, &s2] {
        let (store, barrier) = (Arc::clone(store), Arc::clone(&barrier));
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            store
                .idempotency_begin("race_pub", "key", &Fingerprint([9; 32]), 1)
                .unwrap()
        }));
    }
    let answers: Vec<Begin> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(answers.iter().filter(|a| **a == Begin::New).count(), 1);
    assert!(
        answers
            .iter()
            .all(|a| *a == Begin::New || *a == Begin::Replay { review_id: None })
    );
    b1.shutdown(GRACE).unwrap();
    b2.shutdown(GRACE).unwrap();
}

#[test]
fn racing_duplicate_key_hashes_store_one() {
    let Some(fx) = fixture() else { return };
    let ((b1, s1), (b2, s2)) = two_stores(&fx);
    let barrier = Arc::new(Barrier::new(2));
    let spawn = |store: &Arc<PgStoreState>, publisher: &'static str| {
        let (store, barrier) = (Arc::clone(store), Arc::clone(&barrier));
        std::thread::spawn(move || {
            barrier.wait();
            store.issue_publisher_key(publisher, hash(77), "k", 1)
        })
    };
    let (a, b) = (spawn(&s1, "dup_a"), spawn(&s2, "dup_b"));
    let (a, b) = (a.join().unwrap(), b.join().unwrap());
    assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1);
    assert_eq!(a.err().or(b.err()).unwrap().code, DUPLICATE_KEY_HASH);
    b1.shutdown(GRACE).unwrap();
    b2.shutdown(GRACE).unwrap();
}

#[test]
fn a_call_from_inside_a_tokio_runtime_is_refused_and_changes_nothing() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let before = store.next_generation(1).unwrap();
    let results = fx.rt.block_on(async {
        let s = Arc::clone(&store);
        let blocking = tokio::task::spawn_blocking(move || s.next_generation(2))
            .await
            .unwrap();
        let direct = store.issue_publisher_key("wt_pub", hash(5), "k", 1);
        (blocking, direct)
    });
    for result in [results.0.map(|_| ()), results.1.map(|_| ())] {
        let error = result.unwrap_err();
        assert_eq!(error.code, BACKEND);
        assert!(error.message.contains("pg_bridge.wrong_thread"), "{error}");
    }
    assert_eq!(
        store.lookup_publisher_key(&hash(5)).unwrap(),
        None,
        "nothing was written"
    );
    assert_eq!(
        store.next_generation(3).unwrap(),
        before + 1,
        "the counter did not move"
    );
    bridge.shutdown(GRACE).unwrap();
}

#[test]
fn the_database_itself_refuses_what_the_port_would_not_send() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let a = store.append_audit(&AuditEntry {
        kind: AuditKind::Publish,
        actor: "op".into(),
        publisher_id: None,
        app: None,
        version: None,
        digest: None,
        reason: String::new(),
        at: 1,
    });
    a.unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        for sql in [
            "UPDATE spot_control.spot_store_state_audit SET reason = 'x'",
            "DELETE FROM spot_control.spot_store_state_audit",
            "INSERT INTO spot_control.spot_store_state_key (publisher_id, key_hash, label, issued_at) \
             VALUES ('p', '\\x0102', 'l', 1)",
            "INSERT INTO spot_control.spot_store_state_review (review_id, publisher_id, app, version, \
             digest, status, summary, created_at, updated_at) \
             VALUES ('r', 'p', 'a', '1.0.0', 'd', 'queued', repeat('x', 65537), 1, 1)",
            "INSERT INTO spot_control.spot_store_state_listing (app, version, state, set_by, set_at) \
             VALUES ('a', '1.0.0', 'visible', 'x', 1)",
            "UPDATE spot_control.spot_store_state_meta SET id = 2",
        ] {
            assert!(client.batch_execute(sql).await.is_err(), "accepted: {sql}");
        }
    });
    assert_eq!(store.read_audit(0, 10).unwrap().len(), 1);
    bridge.shutdown(GRACE).unwrap();
}

#[test]
fn an_unreachable_database_is_a_visible_failure_and_no_text_leaks() {
    let Some(fx) = fixture() else { return };
    let mut dead = fx.db.config();
    dead.host = "127.0.0.1".to_owned();
    dead.port = 1;
    let pool = pool_of(dead.clone(), 8);
    let bridge =
        Arc::new(Bridge::new(spot_pg_bridge::BridgeConfig::default(), pool.clone()).unwrap());
    let store = PgStoreState::new(Arc::clone(&bridge), &pool).unwrap();
    let secret = hash(0xAB);
    let errors = [
        store
            .issue_publisher_key("leak_pub", secret, "k", 1)
            .unwrap_err(),
        store.lookup_publisher_key(&secret).map(|_| ()).unwrap_err(),
        store.next_generation(1).map(|_| ()).unwrap_err(),
        store.last_issued_list().map(|_| ()).unwrap_err(),
        store
            .listing(&app("leak_app"), &version("1.0.0"))
            .map(|_| ())
            .unwrap_err(),
    ];
    for error in &errors {
        assert_eq!(
            error.code, BACKEND,
            "a down database is never a default value"
        );
        for text in [format!("{error}"), format!("{error:?}")] {
            for forbidden in [
                dead.password.as_str(),
                dead.host.as_str(),
                dead.user.as_str(),
                dead.dbname.as_str(),
                "postgres://",
                "SELECT",
                "INSERT",
                "UPDATE",
                "leak_pub",
                "leak_app",
                "171",
            ] {
                if !forbidden.is_empty() {
                    assert!(!text.contains(forbidden), "{forbidden:?} leaked in {text}");
                }
            }
        }
    }
    bridge.shutdown(GRACE).unwrap();
}

#[test]
fn the_constructor_refuses_a_current_thread_runtime_and_a_small_pool() {
    let Some(fx) = fixture() else { return };
    let pool = pool_of(fx.db.config(), 10);
    let bridge = Arc::new(Bridge::new(bridge_config(), pool.clone()).unwrap());
    let current = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let error = current
        .block_on(async { PgStoreState::new(Arc::clone(&bridge), &pool) })
        .unwrap_err();
    assert_eq!(error.code(), "store_state.bad_setup");
    let small = pool_of(fx.db.config(), 3);
    assert_eq!(
        PgStoreState::new(bridge, &small).unwrap_err().code(),
        "store_state.bad_setup"
    );
}
