//! Review hardening of the storage-backed sign-in: the throttle treats an unknown name like a
//! real account and resolves an account by name or e-mail, the one-time initial secret signs a
//! user in once and fails closed, and a TOTP step is accepted by exactly one of any number of
//! parallel sign-ins.
//!
//! Every case runs on the in-memory backend and on PostgreSQL (`SPOT_TEST_PG_URL`).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::storage as st;
use common::{CountingHasher, MemEvents};
use spot_http::api::{ApiError, LoginBody, Session};
use spot_login::{KeyStore, LoginConfig, LoginErrorKind, LoginService, UserDirectory};
use spot_sessions::{SessionPolicy, ThrottleConfig};
use spot_storage_contract::{StorageBackend, Value};
use time::OffsetDateTime;

const NOW: i64 = 60;
const PASSWORD: &str = "correct horse battery staple 7731";

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).expect("a valid instant")
}

fn config() -> LoginConfig {
    LoginConfig {
        // Backoff and lockout meet at the third failure, so the third failure locks.
        throttle: ThrottleConfig {
            backoff_after: 3,
            lockout_after: 3,
            window: Duration::from_secs(900),
            backoff_base: Duration::from_secs(1),
            backoff_multiplier: 2,
            backoff_cap: Duration::from_secs(900),
            lockout_duration: Duration::from_secs(1_800),
        },
        session_policy: SessionPolicy::DEFAULT,
        cache_ttl: Duration::ZERO,
        ..LoginConfig::for_tests()
    }
}

fn service<B: StorageBackend>(rig: &st::Rig<B>) -> Arc<LoginService> {
    Arc::new(rig.service_with(
        config(),
        Arc::new(MemEvents::default()),
        Arc::new(CountingHasher::new()),
    ))
}

fn body(user: &str, password: &str) -> LoginBody {
    LoginBody {
        user: user.to_owned(),
        password: password.to_owned(),
        totp: None,
        recovery_code: None,
    }
}

/// Every attempt comes from its own source, so only the account counter can throttle.
async fn login(
    service: &LoginService,
    source: usize,
    user: &str,
    password: &str,
) -> Result<Session, ApiError> {
    service
        .login_with_source(
            &common::ctx(None),
            &body(user, password),
            &format!("10.1.0.{source}"),
        )
        .await
}

fn shape(result: &Result<Session, ApiError>) -> (String, Vec<String>) {
    match result {
        Ok(_) => ("ok".to_owned(), Vec::new()),
        Err(error) => {
            let mut keys: Vec<String> = error.details.keys().map(|key| key.to_string()).collect();
            keys.sort();
            (error.code.as_str().to_owned(), keys)
        }
    }
}

// B1: a locked real account and an unknown name answer identically, attempt for attempt.
async fn case_unknown_name_is_throttled_like_a_real_account<B: StorageBackend>(rig: st::Rig<B>) {
    let service = service(&rig);
    let id = rig.user("ada", "active").await;
    rig.set_password(id, PASSWORD).await;
    for attempt in 0..6 {
        let real = login(&service, attempt * 2, "ada", "not the password").await;
        let unknown = login(&service, attempt * 2 + 1, "nosuchuser", "not the password").await;
        assert_eq!(
            shape(&real),
            shape(&unknown),
            "attempt {attempt}: a real account and an unknown name answered differently"
        );
    }
    let real = login(&service, 100, "ada", PASSWORD).await;
    let unknown = login(&service, 101, "nosuchuser", PASSWORD).await;
    assert_eq!(
        shape(&real).0,
        "api.rate_limited",
        "the real account is locked"
    );
    assert_eq!(shape(&real), shape(&unknown));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_name_is_throttled_like_a_real_account_on_the_fake_backend() {
    case_unknown_name_is_throttled_like_a_real_account(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_name_is_throttled_like_a_real_account_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_unknown_name_is_throttled_like_a_real_account(rig).await;
    }
}

// B2: failures typed as the e-mail lock the account for its name, and the other way round.
async fn case_lockout_is_shared_between_name_and_email<B: StorageBackend>(rig: st::Rig<B>) {
    let service = service(&rig);
    let id = rig.user("ada", "active").await;
    rig.set_password(id, PASSWORD).await;
    for attempt in 0..3 {
        let _ = login(&service, attempt, "ada@example.test", "not the password").await;
    }
    let by_name = login(&service, 10, "ada", PASSWORD).await;
    assert_eq!(
        shape(&by_name).0,
        "api.rate_limited",
        "locked via e-mail failures"
    );
    let by_email = login(&service, 11, "ADA@example.test", PASSWORD).await;
    assert_eq!(shape(&by_email).0, "api.rate_limited");

    let other = rig.user("grace", "active").await;
    rig.set_password(other, PASSWORD).await;
    for attempt in 0..3 {
        let _ = login(&service, 20 + attempt, "grace", "not the password").await;
    }
    let by_email = login(&service, 30, "grace@example.test", PASSWORD).await;
    assert_eq!(
        shape(&by_email).0,
        "api.rate_limited",
        "locked via name failures"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lockout_is_shared_between_name_and_email_on_the_fake_backend() {
    case_lockout_is_shared_between_name_and_email(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lockout_is_shared_between_name_and_email_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_lockout_is_shared_between_name_and_email(rig).await;
    }
}

// B4: the initial secret signs the user in once; wrong, spent and expired ones are refused.
async fn case_initial_secret_signs_in_once<B: StorageBackend>(rig: st::Rig<B>) {
    let service = service(&rig);
    let id = rig.user("ada", "active").await;
    let keys = rig.stores.key_store();
    let secret = st::OpaqueToken::generate();
    keys.set_initial_password(id, &secret, Duration::from_secs(600), NOW)
        .await
        .expect("issued");
    let session = login(&service, 1, "ada", secret.as_str()).await;
    assert!(
        session.is_ok(),
        "the initial secret signs in: {:?}",
        shape(&session)
    );
    let again = login(&service, 2, "ada", secret.as_str()).await;
    assert_eq!(shape(&again).0, "auth.invalid_credentials", "single use");

    let expiring = st::OpaqueToken::generate();
    keys.set_initial_password(id, &expiring, Duration::from_secs(60), NOW)
        .await
        .expect("issued");
    rig.clock.advance(Duration::from_secs(61));
    let late = login(&service, 3, "ada", expiring.as_str()).await;
    assert_eq!(shape(&late).0, "auth.invalid_credentials", "expired");

    let other = rig.user("bob", "active").await;
    let theirs = st::OpaqueToken::generate();
    keys.set_initial_password(other, &theirs, Duration::from_secs(600), NOW + 61)
        .await
        .expect("issued");
    let stolen = login(&service, 4, "ada", theirs.as_str()).await;
    assert_eq!(
        shape(&stolen).0,
        "auth.invalid_credentials",
        "another user's secret"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_initial_secret_signs_in_once_on_the_fake_backend() {
    case_initial_secret_signs_in_once(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_initial_secret_signs_in_once_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_initial_secret_signs_in_once(rig).await;
    }
}

// B4: a missing or non-date expiry is a malformed row and never means "never expires".
async fn case_missing_expiry_fails_closed<B: StorageBackend>(rig: st::Rig<B>) {
    let id = rig.user("ada", "active").await;
    let keys = rig.stores.key_store();
    let secret = st::OpaqueToken::generate();
    let key_id = keys
        .set_initial_password(id, &secret, Duration::from_secs(600), NOW)
        .await
        .expect("issued");
    let row = rig.api_key_row_id(&key_id).await;
    rig.poke(st::API_KEY, row, "expires_at", Value::Null).await;
    let refused = KeyStore::consume_initial_secret(&keys, id, &secret, NOW + 1)
        .await
        .expect_err("a row without an expiry is refused");
    assert_eq!(refused.kind, LoginErrorKind::InitialSecretInvalid);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_expiry_fails_closed_on_the_fake_backend() {
    case_missing_expiry_fails_closed(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_expiry_fails_closed_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_missing_expiry_fails_closed(rig).await;
    }
}

// B5: the same TOTP step is accepted by exactly one of several parallel writers.
async fn case_one_totp_step_has_one_winner<B: StorageBackend>(rig: st::Rig<B>) {
    let id = rig.user("ada", "active").await;
    rig.enrol_totp(id).await;
    let directory = Arc::new(rig.stores.user_directory());
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let directory = directory.clone();
        tasks.push(tokio::spawn(async move {
            directory.record_totp_step(id, 5).await
        }));
    }
    let mut won = 0;
    for task in tasks {
        match task.await.expect("the task runs") {
            Ok(()) => won += 1,
            Err(error) => assert_eq!(error.kind, LoginErrorKind::TotpReplayed, "{error:?}"),
        }
    }
    assert_eq!(won, 1, "exactly one writer accepts the step");
    let record = directory.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(record.totp_last_step, Some(5));
    let older = directory.record_totp_step(id, 4).await;
    assert!(older.is_err(), "an older step is a replay");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_totp_step_has_one_winner_on_the_fake_backend() {
    case_one_totp_step_has_one_winner(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_totp_step_has_one_winner_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_one_totp_step_has_one_winner(rig).await;
    }
}
