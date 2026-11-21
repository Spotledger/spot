//! The store over real PostgreSQL: the registry index and the store state behind the shared
//! bridge, called only from the server's own OS threads. The tests skip (with a message) unless
//! `SPOT_TEST_PG_URL` is set, and fail when `SPOT_REQUIRE_PG=1` and it is not.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::http::StatusCode;
use common::{Harness, bundle, open, signature_of, standard_parts};
use serde_json::json;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_registry_pg::PgIndexStore;
use spot_store_state_pg::PgStoreState;

static NAMES: AtomicUsize = AtomicUsize::new(0);

/// Everything that must stay alive while a test runs.
struct Pg {
    harness: Harness,
    index: Arc<PgIndexStore>,
    state: Arc<PgStoreState>,
    _db: TestDb,
    _backend: PgBackend,
    _bridge: Arc<Bridge>,
    _rt: tokio::runtime::Runtime,
}

fn pg() -> Option<Pg> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let db = rt.block_on(TestDb::create())?;
    let mut config = db.config();
    config.pool_max_size = 16;
    let backend = PgBackend::connect_unchecked(config).unwrap();
    let bridge = Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: format!("store_server_pg_{}", NAMES.fetch_add(1, Ordering::SeqCst)),
                workers: 2,
                max_in_flight: 12,
                queue_capacity: 64,
                default_deadline: Duration::from_secs(5),
                max_deadline: Duration::from_secs(30),
            },
            &backend,
        )
        .unwrap(),
    );
    let pool = backend.pool();
    let index = Arc::new(PgIndexStore::new(Arc::clone(&bridge), &pool).unwrap());
    index.prepare().unwrap();
    let state = Arc::new(PgStoreState::new(Arc::clone(&bridge), &pool).unwrap());
    state.prepare().unwrap();
    let harness = Harness::on(index.clone(), state.clone(), |_| {});
    Some(Pg {
        harness,
        index,
        state,
        _db: db,
        _backend: backend,
        _bridge: bridge,
        _rt: rt,
    })
}

#[test]
fn publish_approve_search_download_and_a_restart_over_postgres() {
    let Some(pg) = pg() else {
        return;
    };
    let harness = &pg.harness;
    let token = harness.issue_key(1);
    let bytes = bundle("pg_round_trip", "1.0.0", "pg_round_trip");
    let reply = harness.publish(&token, "pg_round_trip", &bytes, 1);
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.json());
    // Fail closed until a person approves.
    assert_eq!(
        harness
            .get("/store/v1/apps")
            .data()
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        harness.approve("pg_round_trip", "1.0.0").status,
        StatusCode::OK
    );

    let search = harness.get("/store/v1/apps");
    assert_eq!(search.data().as_array().unwrap().len(), 1);
    let download = harness.get("/store/v1/apps/pg_round_trip/versions/1.0.0/bundle");
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(open(&download.body).digest(), open(&bytes).digest());

    // A revoked publisher is on the next list, over the real state store.
    let revoke = harness.admin_post(
        "/admin/v1/publishers/publisher1/revoke",
        Some(json!({ "reason": "abuse" })),
    );
    assert_eq!(revoke.status, StatusCode::OK, "{:?}", revoke.json());
    let list = harness.get("/store/v1/revocations");
    let text = String::from_utf8(list.body.clone()).unwrap();
    assert!(text.contains("\"publisher1\""));
    let next = bundle("pg_round_trip", "2.0.0", "pg_round_trip");
    let fresh = harness.issue_key(1);
    assert_eq!(
        harness.publish(&fresh, "pg_round_trip", &next, 1).code(),
        "store.publisher_revoked"
    );

    // A second server process over the same database serves the same list bytes.
    let second = Harness::on(pg.index.clone(), pg.state.clone(), |_| {});
    assert_eq!(second.get("/store/v1/revocations").body, list.body);
    // The audit log survived in the database.
    assert!(
        second
            .audit()
            .iter()
            .any(|(kind, reason)| kind == "revoke_publisher" && reason == "abuse")
    );
}

#[test]
fn concurrent_publishes_of_one_version_have_exactly_one_winner_over_postgres() {
    let Some(pg) = pg() else {
        return;
    };
    let harness = &pg.harness;
    let token = harness.issue_key(1);
    let requests: Vec<_> = (0..5)
        .map(|n| {
            let bytes = bundle("pg_racing", "1.0.0", &format!("variant {n}"));
            let parts = standard_parts(&bytes, &signature_of(&bytes, 1), 1);
            harness.publish_request(Some(&token), "pg_racing", &parts, &[])
        })
        .collect();
    let replies: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = requests
            .into_iter()
            .map(|request| scope.spawn(|| harness.send(request)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let accepted = replies
        .iter()
        .filter(|reply| reply.status == StatusCode::ACCEPTED)
        .count();
    assert_eq!(
        accepted,
        1,
        "{:?}",
        replies.iter().map(|r| r.status).collect::<Vec<_>>()
    );
    for reply in replies.iter().filter(|r| r.status != StatusCode::ACCEPTED) {
        assert_eq!(reply.code(), "store.version_not_monotonic");
    }
    assert_eq!(harness.index.digests().unwrap().len(), 1);
}
