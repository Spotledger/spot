//! The store on a real PostgreSQL. Skips with a `SKIPPED:` line when `SPOT_TEST_PG_URL` is unset;
//! `SPOT_REQUIRE_PG=1` turns a missing server into a failure.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_deep_access_pg::{
    ConsentRow, CredentialRow, KeyRow, PgDeepAccessStore, RequestRow, RequestStatus,
};
use spot_postgres::testing::TestDb;
use tokio_postgres::NoTls;

fn pool(db: &TestDb, app: &str) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.application_name = Some(app.to_owned());
    cfg.pool = Some(PoolConfig::new(8));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

async fn store(db: &TestDb, tenant: &str) -> PgDeepAccessStore {
    let store = PgDeepAccessStore::new(pool(db, "deep_access_test"), tenant).unwrap();
    store.prepare().await.unwrap();
    store
}

fn request(id: &str) -> RequestRow {
    RequestRow {
        id: id.to_owned(),
        request: "{\"id\":\"r\"}".to_owned(),
        request_hash: "ab".repeat(32),
        status: RequestStatus::Pending,
        requested_at: 100,
        expires_at: 1_000,
    }
}

#[tokio::test]
async fn a_request_is_stored_once_and_never_overwritten() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let s = store(&db, "acme").await;
    assert!(s.put_request(&request("r1"), 100).await.unwrap());
    let mut other = request("r1");
    other.request = "{\"changed\":true}".to_owned();
    assert!(!s.put_request(&other, 101).await.unwrap());
    assert_eq!(
        s.get_request("r1").await.unwrap().unwrap().request,
        request("r1").request
    );
}

#[tokio::test]
async fn another_tenant_sees_nothing_and_a_cross_tenant_id_is_a_missing_one() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let acme = store(&db, "acme").await;
    let other = store(&db, "other").await;
    acme.put_request(&request("r1"), 100).await.unwrap();
    assert!(other.get_request("r1").await.unwrap().is_none());
    assert!(
        other
            .list_requests(None, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !other
            .transition(
                "r1",
                &[RequestStatus::Pending],
                RequestStatus::Approved,
                101
            )
            .await
            .unwrap()
    );
    // The same id can exist for both tenants.
    assert!(other.put_request(&request("r1"), 100).await.unwrap());
}

#[tokio::test]
async fn transitions_only_follow_the_allowed_source_states() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let s = store(&db, "acme").await;
    s.put_request(&request("r1"), 100).await.unwrap();
    assert!(
        s.transition("r1", &[RequestStatus::Pending], RequestStatus::Revoked, 101)
            .await
            .unwrap()
    );
    // A revoked request cannot be approved.
    assert!(
        !s.transition(
            "r1",
            &[RequestStatus::Pending],
            RequestStatus::Approved,
            102
        )
        .await
        .unwrap()
    );
    assert_eq!(
        s.get_request("r1").await.unwrap().unwrap().status,
        RequestStatus::Revoked
    );
}

#[tokio::test]
async fn a_nonce_is_consumed_once_even_under_a_race_and_after_a_restart() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let a = store(&db, "acme").await;
    let b = store(&db, "acme").await; // a second node
    let (x, y) = tokio::join!(
        a.consume_nonce("k1", [7; 16]),
        b.consume_nonce("k1", [7; 16])
    );
    let wins = [x.unwrap(), y.unwrap()].iter().filter(|w| **w).count();
    assert_eq!(wins, 1);
    // A restart is a new store over the same database.
    let c = store(&db, "acme").await;
    assert!(!c.consume_nonce("k1", [7; 16]).await.unwrap());
    // Another key or another tenant has its own nonce space.
    assert!(c.consume_nonce("k2", [7; 16]).await.unwrap());
    let other = store(&db, "other").await;
    assert!(other.consume_nonce("k1", [7; 16]).await.unwrap());
}

#[tokio::test]
async fn the_first_revocation_stays() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let s = store(&db, "acme").await;
    s.revoke([1; 16], "alice", "first reason", 10)
        .await
        .unwrap();
    s.revoke([1; 16], "bob", "second reason", 20).await.unwrap();
    let (by, reason, at) = s.revocation([1; 16]).await.unwrap().unwrap();
    assert_eq!(
        (by.as_str(), reason.as_str(), at),
        ("alice", "first reason", 10)
    );
    assert!(s.revocation([2; 16]).await.unwrap().is_none());
    let other = store(&db, "other").await;
    assert!(other.revocation([1; 16]).await.unwrap().is_none());
}

#[tokio::test]
async fn consents_are_one_per_approver() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let s = store(&db, "acme").await;
    let row = |approver: &str| ConsentRow {
        request_id: "r1".to_owned(),
        approver: approver.to_owned(),
        approver_key: "key".to_owned(),
        record: "{}".to_owned(),
        signature: vec![1, 2, 3],
        receipt_hash: "cd".repeat(32),
        recorded_at: 50,
    };
    assert!(s.record_consent(&row("alice")).await.unwrap());
    assert!(!s.record_consent(&row("alice")).await.unwrap());
    assert!(s.record_consent(&row("bob")).await.unwrap());
    assert_eq!(s.consents("r1").await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_credential_row_holds_a_digest_and_the_revocation_shows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let s = store(&db, "acme").await;
    let row = CredentialRow {
        session_id: [9; 16],
        request_id: "r1".to_owned(),
        request_hash: "ab".repeat(32),
        receipt_hash: "cd".repeat(32),
        scope: "read all documents".to_owned(),
        starts_at: 10,
        expires_at: 500,
        issued_at: 10,
    };
    s.record_credential(&row, [0xAA; 32]).await.unwrap();
    let listed = s.credentials(10).await.unwrap();
    assert_eq!(listed, vec![(row.clone(), false)]);
    s.revoke([9; 16], "alice", "done", 20).await.unwrap();
    assert!(s.credentials(10).await.unwrap()[0].1);
    // The table has no column that could hold the secret itself.
    let c = pool(&db, "check").get().await.unwrap();
    let cols = c
        .query(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_name = 'spot_deep_access_credentials'",
            &[],
        )
        .await
        .unwrap();
    assert!(cols.iter().all(|r| r.get::<_, String>(0) != "secret"));
}

#[tokio::test]
async fn the_sweep_expires_requests_and_old_nonces_but_keeps_history() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let s = store(&db, "acme").await;
    s.put_request(&request("r1"), 100).await.unwrap();
    let (expired, _) = s.sweep(2_000, 86_400, 100).await.unwrap();
    assert_eq!(expired, 1);
    assert_eq!(
        s.get_request("r1").await.unwrap().unwrap().status,
        RequestStatus::Expired
    );
    // Revocations stay after a sweep.
    s.revoke([3; 16], "alice", "r", 5).await.unwrap();
    s.sweep(10_000_000, 1, 100).await.unwrap();
    assert!(s.revocation([3; 16]).await.unwrap().is_some());
}

#[tokio::test]
async fn keys_are_enrolled_once_per_tenant() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let s = store(&db, "acme").await;
    let key = KeyRow {
        key_id: "kid".to_owned(),
        identity: "alice".to_owned(),
        public_key: [5; 32],
        is_owner: true,
        enrolled_by: "admin".to_owned(),
        enrolled_at: 1,
    };
    assert!(s.enrol_key(&key).await.unwrap());
    assert!(!s.enrol_key(&key).await.unwrap());
    assert_eq!(s.keys().await.unwrap(), vec![key]);
    assert!(store(&db, "other").await.keys().await.unwrap().is_empty());
}

#[tokio::test]
async fn bad_input_is_refused_before_the_database() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let s = store(&db, "acme").await;
    let mut bad = request(&"x".repeat(201));
    assert!(s.put_request(&bad, 1).await.is_err());
    bad.id = String::new();
    assert!(s.put_request(&bad, 1).await.is_err());
    assert!(PgDeepAccessStore::new(pool(&db, "t"), "").is_err());
}
