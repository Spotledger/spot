//! The real router over the PostgreSQL store: a keyed `POST` runs once, a restarted router and a
//! second node still replay it, a different body is a conflict, and with the database gone the
//! request is refused with 503 and the service ran zero times.
//!
//! The router, the fake services and the request helpers are `spot-http`'s own test support,
//! included from its source tree so both suites exercise one plumbing.
#![allow(missing_docs, dead_code, clippy::unwrap_used, clippy::expect_used)]

#[path = "../../../framework/spot-http/tests/support/mod.rs"]
mod support;

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use deadpool_postgres::Pool;
use serde_json::json;
use spot_core::{ManualClock, SiteId};
use spot_http::api::{
    ApiServices, IdempotencyStore, MemoryFilesPort, MemoryIdempotencyStore, router,
};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_idempotency_pg::PgIdempotencyStore;
use spot_postgres::testing::TestDb;
use support::Must;
use support::fake::{FakeApp, TOKEN_ALICE};
use support::harness::{self, App, HOST, SITE};
use support::permissions::{FakePermissions, FakeReveal};
use tower::ServiceExt as _;

/// A router (a node) over `fake` with `store`. Several nodes may share one fake: the service
/// they all run is then one counter.
fn node(fake: &Arc<FakeApp>, store: Arc<dyn IdempotencyStore>, clock: &Arc<ManualClock>) -> App {
    let permissions = Arc::new(FakePermissions::new());
    let reveal = Arc::new(FakeReveal::new());
    let site = SiteId::new(SITE).must();
    let services = ApiServices::new(
        fake.clone(),
        fake.clone(),
        fake.clone(),
        fake.clone(),
        store,
    )
    .with_permissions(permissions.clone())
    .with_reveal(reveal.clone())
    .with_files(Arc::new(MemoryFilesPort::new(1024)));
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(router(services))
    .build();
    App {
        router,
        fake: fake.clone(),
        permissions,
        reveal,
        store: Arc::new(MemoryIdempotencyStore::new(clock.clone())),
        clock: clock.clone(),
    }
}

async fn pg_node(
    db: &TestDb,
    fake: &Arc<FakeApp>,
    clock: &Arc<ManualClock>,
    name: &str,
) -> (App, Pool) {
    let pool = common::pool(db, 16, name);
    let store = PgIdempotencyStore::new(pool.clone(), clock.clone(), common::config()).unwrap();
    store.prepare().await.unwrap();
    (node(fake, Arc::new(store), clock), pool)
}

fn body(name: &str) -> serde_json::Value {
    json!({ "name": name })
}

const PATH: &str = "/api/v1/resource/customer";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_post_with_one_key_runs_the_service_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let fake = FakeApp::new(clock.clone());
    let (app, _pool) = pg_node(&db, &fake, &clock, "idem_router_once").await;
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("once-1")];
    let first = harness::call(&app, "POST", PATH, &head, Some(body("ACME once"))).await;
    first.expect_status(201, "the first create runs");
    assert!(first.header("idempotency-replayed").is_none());
    let second = harness::call(&app, "POST", PATH, &head, Some(body("ACME once"))).await;
    second.expect_status(201, "the retry replays");
    assert_eq!(second.header("idempotency-replayed"), Some("true"));
    assert_eq!(
        second.raw, first.raw,
        "the replay is byte for byte the first answer"
    );
    assert_eq!(fake.stored_count_of("customer"), 1, "the service ran once");

    // The stored row is the response exactly, scoped by the credential hash, never the token.
    let rows = db
        .raw_client()
        .await
        .query(
            "SELECT credential, status, body, headers::text FROM spot_idempotency_key",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let credential: String = rows[0].get(0);
    let stored_body: Vec<u8> = rows[0].get(2);
    assert_eq!(credential.len(), 64);
    assert!(!credential.contains(TOKEN_ALICE));
    assert_eq!(
        stored_body,
        first.raw.as_bytes(),
        "stored bytes are the sent bytes"
    );
    let headers: String = rows[0].get(3);
    assert!(!headers.to_lowercase().contains("set-cookie"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_router_still_replays() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let first_fake = FakeApp::new(clock.clone());
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("restart-1")];
    let first = {
        let (app, pool) = pg_node(&db, &first_fake, &clock, "idem_router_before").await;
        let first = harness::call(&app, "POST", PATH, &head, Some(body("ACME restart"))).await;
        first.expect_status(201, "created before the restart");
        drop(app);
        pool.close();
        first
    };
    // A new Router and a new store over the same database; the service has no memory of it.
    let second_fake = FakeApp::new(clock.clone());
    let (app, _pool) = pg_node(&db, &second_fake, &clock, "idem_router_after").await;
    let replay = harness::call(&app, "POST", PATH, &head, Some(body("ACME restart"))).await;
    replay.expect_status(201, "the retry after the restart");
    assert_eq!(replay.header("idempotency-replayed"), Some("true"));
    assert_eq!(replay.raw, first.raw);
    assert_eq!(
        second_fake.stored_count_of("customer"),
        0,
        "the restarted node did not run the command again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_different_body_under_the_same_key_is_a_conflict() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let fake = FakeApp::new(clock.clone());
    let (app, _pool) = pg_node(&db, &fake, &clock, "idem_router_conflict").await;
    let head = [
        harness::bearer(TOKEN_ALICE),
        harness::idem_key("conflict-1"),
    ];
    harness::call(&app, "POST", PATH, &head, Some(body("ACME one")))
        .await
        .expect_status(201, "first");
    let other = harness::call(&app, "POST", PATH, &head, Some(body("ACME two"))).await;
    other.expect_status(409, "same key, different body");
    assert_eq!(other.error_code(), "api.idempotency_conflict");
    assert_eq!(fake.stored_count_of("customer"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_caller_never_sees_a_stored_response() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let fake = FakeApp::new(clock.clone());
    let (app, _pool) = pg_node(&db, &fake, &clock, "idem_router_scope").await;
    let key = harness::idem_key("shared-key");
    let alice = [harness::bearer(TOKEN_ALICE), key.clone()];
    harness::call(&app, "POST", PATH, &alice, Some(body("ACME alice")))
        .await
        .expect_status(201, "alice");
    // Another credential using the same key and body is a fresh request, not alice's replay.
    let other_token = support::fake::TOKEN_BOB;
    let bob = [harness::bearer(other_token), key];
    let bob_call = harness::call(&app, "POST", PATH, &bob, Some(body("ACME alice"))).await;
    bob_call.expect_status(201, "bob's own request");
    assert!(bob_call.header("idempotency-replayed").is_none());
    assert_eq!(fake.stored_count_of("customer"), 2, "both ran");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_receiving_the_same_keyed_post_at_once_run_it_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let fake = FakeApp::new(clock.clone());
    let (node_a, _pa) = pg_node(&db, &fake, &clock, "idem_router_a").await;
    let (node_b, _pb) = pg_node(&db, &fake, &clock, "idem_router_b").await;
    let head = [
        harness::bearer(TOKEN_ALICE),
        harness::idem_key("two-nodes-1"),
    ];
    let (a, b) = tokio::join!(
        harness::call(&node_a, "POST", PATH, &head, Some(body("ACME two nodes"))),
        harness::call(&node_b, "POST", PATH, &head, Some(body("ACME two nodes"))),
    );
    a.expect_status(201, "node a");
    b.expect_status(201, "node b");
    assert_eq!(a.raw, b.raw, "both nodes describe the same document");
    assert_eq!(
        fake.stored_count_of("customer"),
        1,
        "one command ran across both nodes"
    );
    let replays = [
        a.header("idempotency-replayed"),
        b.header("idempotency-replayed"),
    ];
    assert_eq!(replays.iter().filter(|r| r.is_some()).count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_database_gone_a_keyed_request_is_refused_and_nothing_runs() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let fake = FakeApp::new(clock.clone());
    let (app, pool) = pg_node(&db, &fake, &clock, "idem_router_down").await;
    pool.close();
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("down-1")];
    let call = harness::call(&app, "POST", PATH, &head, Some(body("ACME down"))).await;
    call.expect_status(503, "the store is down");
    assert_eq!(call.error_code(), "server.unavailable");
    assert_eq!(call.header("retry-after"), Some("1"));
    assert_eq!(
        fake.stored_count_of("customer"),
        0,
        "the service ran zero times"
    );
    assert!(
        !call.raw.to_lowercase().contains("postgres"),
        "no driver text: {}",
        call.raw
    );
    // A request without a key does not use the store and still works.
    let free = harness::call(
        &app,
        "POST",
        PATH,
        &[harness::bearer(TOKEN_ALICE)],
        Some(body("ACME free")),
    )
    .await;
    free.expect_status(201, "no key, no store");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_database_that_stops_answering_refuses_the_request_in_bounded_time() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let fake = FakeApp::new(clock.clone());
    let pool = common::pool(&db, 8, "idem_router_hung");
    let config = common::config().with_operation_timeout(Duration::from_millis(400));
    let store = PgIdempotencyStore::new(pool, clock.clone(), config).unwrap();
    store.prepare().await.unwrap();
    let app = node(&fake, Arc::new(store), &clock);
    let mut holder = db.raw_client().await;
    let tx = holder.transaction().await.unwrap();
    tx.batch_execute("LOCK TABLE spot_idempotency_key IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("hung-1")];
    let call = harness::call(&app, "POST", PATH, &head, Some(body("ACME hung"))).await;
    call.expect_status(503, "the store does not answer");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(fake.stored_count_of("customer"), 0);
    tx.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_function_call_is_reserved_then_released_and_leaves_no_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let fake = FakeApp::new(clock.clone());
    let (app, _pool) = pg_node(&db, &fake, &clock, "idem_router_none").await;
    let head = [harness::bearer(TOKEN_ALICE), harness::idem_key("fn-1")];
    for _ in 0..2 {
        harness::call(
            &app,
            "POST",
            "/api/v1/method/spot/sales/fn",
            &head,
            Some(json!({ "args": {} })),
        )
        .await;
        assert_eq!(
            common::row_count(&db).await,
            0,
            "an app-code response is never written; the key is released"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_routes_that_carry_secrets_never_reach_the_table() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let fake = FakeApp::new(clock.clone());
    let (app, _pool) = pg_node(&db, &fake, &clock, "idem_router_secret").await;
    let key = harness::idem_key("secret-1");
    harness::call(
        &app,
        "POST",
        "/api/v1/auth/login",
        std::slice::from_ref(&key),
        Some(json!({ "user": "alice@acme.com", "password": "correct horse" })),
    )
    .await
    .expect_status(200, "login");
    harness::call(
        &app,
        "POST",
        "/api/v1/auth/logout",
        &[harness::bearer(TOKEN_ALICE), key.clone()],
        None,
    )
    .await;
    assert_eq!(
        common::row_count(&db).await,
        0,
        "login and logout wrote nothing"
    );
    let dump: String = db
        .raw_client()
        .await
        .query_one(
            "SELECT coalesce(string_agg(t::text, ''), '') FROM spot_idempotency_key t",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!dump.contains("correct horse"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_upload_record_is_stored_under_the_upload_class() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = common::clock();
    let fake = FakeApp::new(clock.clone());
    let (app, _pool) = pg_node(&db, &fake, &clock, "idem_router_upload").await;
    let send = |app: &App| {
        let router = app.router.clone();
        async move {
            let response = router
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/files")
                        .header(header::HOST, HOST)
                        .header("authorization", format!("Bearer {TOKEN_ALICE}"))
                        .header("idempotency-key", "upload-1")
                        .header("content-type", "application/octet-stream")
                        .header("content-disposition", "attachment; filename=\"a.txt\"")
                        .body(Body::from(b"hello".to_vec()))
                        .must(),
                )
                .await
                .must();
            let status = response.status().as_u16();
            let replayed = response.headers().contains_key("idempotency-replayed");
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .must();
            (status, replayed, bytes.to_vec())
        }
    };
    let (status, replayed, first) = send(&app).await;
    assert_eq!(status, 201);
    assert!(!replayed);
    let (status, replayed, second) = send(&app).await;
    assert_eq!(status, 201);
    assert!(replayed, "the same upload with the same key replays");
    assert_eq!(first, second);
    let expires = common::expires_at(&db, "upload-1").await.unwrap();
    assert_eq!(
        expires,
        clock_unix(&clock) + 86_400,
        "the upload class takes the default retention"
    );
}

fn clock_unix(clock: &Arc<ManualClock>) -> i64 {
    spot_core::Clock::now_utc(&**clock).unix_timestamp()
}
