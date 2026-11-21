//! The stores of `spot-login` on a real PostgreSQL pool of one connection: while the only
//! connection is held, every store call that has to begin a transaction waits its
//! `pool_wait_timeout` and then returns the typed retryable `Unavailable` (never a plain
//! `StoreFailed`); a store error of another kind is still an ordinary `StoreFailed`; and as soon
//! as the connection is free again the same calls succeed (WP-PRD-01.T43).
//!
//! Skipped, with a printed reason, when `SPOT_TEST_PG_URL` is unset. Nothing waits on a sleep:
//! each refusal is the pool's own wait running out (200 ms).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::time::Duration;

use common::storage as st;
use spot_http::api::{Code, ErrorCode};
use spot_login::{KeyStore, LoginError, LoginErrorKind, UserDirectory};
use spot_sessions::{SessionStore, ThrottleStore, UserKey};
use st::{Isolation, StorageBackend, Transaction, TxOptions};

/// A PostgreSQL rig whose pool holds one connection and waits 200 ms for it.
async fn one_connection_rig() -> Option<st::Rig<st::PgBackend>> {
    let db = st::TestDb::create().await?;
    let mut config = db.config();
    config.pool_max_size = 1;
    config.pool_wait_timeout = Duration::from_millis(200);
    let backend = st::PgBackend::connect(config)
        .await
        .expect("a PostgreSQL 18 connection");
    let rig = st::build(backend, Some(db), st::start());
    rig.engine.prepare().await.expect("the PostgreSQL schema");
    Some(rig)
}

fn assert_unavailable(error: &LoginError, what: &str) {
    assert!(error.is_unavailable(), "{what}: {error}");
    assert_eq!(error.kind, LoginErrorKind::StoreFailed, "{what}");
    let api = error.to_api_error();
    assert_eq!(
        api.code,
        ErrorCode::Registry(Code::ServerUnavailable),
        "{what}"
    );
    assert_eq!(api.status().as_u16(), 503, "{what}");
    assert!(api.retryable(), "{what}");
    assert!(
        api.details.get("retry_after").and_then(|v| v.as_u64()) >= Some(1),
        "{what}: a positive retry_after"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_store_call_that_begins_a_transaction_is_unavailable_while_the_only_connection_is_held()
 {
    let Some(rig) = one_connection_rig().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let user = rig.user("alice", "active").await;
    let directory = rig.stores.user_directory();
    let sessions = rig.stores.session_store();
    let keys = rig.stores.key_store();
    let throttle = rig.stores.throttle_store(Duration::from_secs(900));
    let key = UserKey::new(user.to_string()).unwrap();

    // Hold the only connection: an open transaction through the same backend.
    let held = rig
        .engine
        .backend()
        .begin(
            rig.catalog.clone(),
            TxOptions::new(st::n("hold_the_only_connection"), Isolation::ReadCommitted),
        )
        .await
        .expect("the pool's only connection");

    let by_name = directory.find_by_identifier("alice").await.unwrap_err();
    assert_unavailable(&by_name, "find_by_identifier");
    let by_id = directory.find_by_id(user).await.unwrap_err();
    assert_unavailable(&by_id, "find_by_id");
    let step = directory.record_totp_step(user, 5).await.unwrap_err();
    assert_unavailable(&step, "record_totp_step");
    let found = keys.find_by_key_id("some-key-id").await.unwrap_err();
    assert_unavailable(&found, "find_by_key_id");
    let listed = LoginError::from(sessions.list_for_user(&key).await.unwrap_err());
    assert_unavailable(&listed, "list_for_user");
    let purged = LoginError::from(sessions.purge_expired(0, 10).await.unwrap_err());
    assert_unavailable(&purged, "purge_expired");
    let account = LoginError::from(throttle.get(true, b"alice").await.unwrap_err());
    assert_unavailable(&account, "the account counter");
    let source = LoginError::from(throttle.get(false, b"source-digest").await.unwrap_err());
    assert_unavailable(&source, "the source counter");

    // The wait ended, the connection is free again: the same call succeeds.
    let _ = held.rollback().await;
    assert!(
        directory
            .find_by_identifier("alice")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_error_of_another_kind_is_still_an_ordinary_store_failure() {
    let Some(rig) = one_connection_rig().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    // The connection is free: the call reaches the database and the user simply does not exist.
    let error = rig
        .stores
        .user_directory()
        .record_totp_step(st::RecordId::new_v7(), 5)
        .await
        .unwrap_err();
    assert_eq!(error.kind, LoginErrorKind::StoreFailed);
    assert!(!error.is_unavailable(), "{error}");
    let api = error.to_api_error();
    assert_eq!(api.code, ErrorCode::Registry(Code::HttpInternal));
    assert!(!api.retryable());
}
